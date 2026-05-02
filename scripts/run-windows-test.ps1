# run-windows-test.ps1 -- local mirror of validate-mkfs-windows in CI.
#
# Designed for parity with `.github/workflows/ci.yml`'s validate-mkfs-windows
# job: build mkfs_ntfs.exe, format an nfs.img, wrap in a GPT-partitioned
# VHDX, mount, run chkdsk + a Microsoft format.com reference, dump every
# diagnostic CI dumps, into ./diag/.
#
# Differences from CI:
#  * No actions/checkout -- assumes the source is already in pwd.
#  * No artifact upload -- diag/ stays in pwd; orchestrator scp's it back.
#  * No tag triggers -- runs whenever invoked.
#
# Mac-side orchestrator (`scripts/test-windows-local.sh`) handles source
# transfer and result fetch.

param(
    [int]$VolumeSizeMb = 256,
    [int]$WrapperSizeMb = 384,
    [string]$Label = "CITEST"
)

$ErrorActionPreference = "Stop"
$env:PATH = "$env:USERPROFILE\.cargo\bin;C:\Program Files\Cloudbase Solutions\QEMU\bin;$env:PATH"

# rust-toolchain.toml pins channel = "1.94.1" without a host triple, so
# rustup picks the default host. On Windows ARM64 that's MSVC, which we
# don't have installed (intentionally — see install docs). Override via
# RUSTUP_TOOLCHAIN to use the gnullvm toolchain for this run only.
# (Also suppresses rustup's "info: syncing channel updates" stderr noise
# that PowerShell's strict mode treats as a fatal error.)
$env:RUSTUP_TOOLCHAIN = "stable-aarch64-pc-windows-gnullvm"

# Workspace already contains the source; clean prior diag/ + artefacts.
Remove-Item -Recurse -Force diag, nfs.img, wrapper.vhdx, reference.vhdx -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Path diag -Force | Out-Null

# --- Build ------------------------------------------------------------
Write-Host "[1/6] Building mkfs_ntfs.exe ..."
# Cargo writes informational "Compiling foo" lines to stderr, which under
# $ErrorActionPreference=Stop becomes a fatal terminating error. Run the
# build in a sub-scope where stderr is redirected to a file we capture
# regardless of exit, and check $LASTEXITCODE explicitly.
& cmd.exe /c "cargo build --release --bin mkfs_ntfs --quiet > diag\build.txt 2>&1"
if ($LASTEXITCODE -ne 0) {
    Copy-Item diag\build.txt diag\build-failed.txt
    Get-Content diag\build-failed.txt | Out-Host
    throw "cargo build failed -- see diag/build-failed.txt"
}
"OK" | Tee-Object diag/build-status.txt | Out-Null

# --- Generate nfs.img -------------------------------------------------
Write-Host "[2/6] Generating nfs.img and wrapping in VHDX ..."
$rawSize = $VolumeSizeMb * 1MB
fsutil file createnew nfs.img $rawSize | Out-Null
./target/release/mkfs_ntfs.exe -L $Label --serial deadbeefcafe1234 nfs.img |
    Tee-Object diag/mkfs-output.txt

# Pre-wrap BPB dump (ground-truth view of mkfs_ntfs output).
$imgBytes = [System.IO.File]::ReadAllBytes("$pwd\nfs.img")
$hex = ($imgBytes[0..63] | ForEach-Object { '{0:X2}' -f $_ }) -join ' '
"first 64 bytes of nfs.img:`n$hex" | Out-File diag/nfs-img-hex.txt
$bps = [System.BitConverter]::ToUInt16($imgBytes, 0x0B)
$spc = $imgBytes[0x0D]
$totalSectors = [System.BitConverter]::ToUInt64($imgBytes, 0x28)
$mftLcn = [System.BitConverter]::ToUInt64($imgBytes, 0x30)
"nfs.img BPB: bytes_per_sector=$bps, sectors_per_cluster=$spc, total_sectors=$totalSectors, mft_lcn=$mftLcn" |
    Tee-Object diag/nfs-img-bpb.txt

# --- Wrap in GPT-partitioned VHDX -------------------------------------
qemu-img create -f vhdx -o subformat=fixed wrapper.vhdx "${WrapperSizeMb}M" |
    Out-File diag/qemu-create-wrapper.txt
fsutil sparse setflag wrapper.vhdx 0 | Out-Null

$vhd = Mount-DiskImage -ImagePath "$pwd\wrapper.vhdx" -PassThru
Start-Sleep -Seconds 2
Initialize-Disk -Number $vhd.Number -PartitionStyle GPT
Start-Sleep -Seconds 2
$disk = Get-Disk -Number $vhd.Number
$part = New-Partition -DiskNumber $vhd.Number -UseMaximumSize -AssignDriveLetter:$false
if ($part.Size -lt $rawSize) { throw "partition smaller than raw image -- bump wrapper" }

# Raw write nfs.img into partition area at $part.Offset.
$rawPath = "\\.\PhysicalDrive$($disk.Number)"
$bytes = [System.IO.File]::ReadAllBytes("$pwd\nfs.img")
$fs = [System.IO.File]::Open($rawPath, [System.IO.FileMode]::Open,
    [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::ReadWrite)
try {
    $fs.Seek($part.Offset, [System.IO.SeekOrigin]::Begin) | Out-Null
    $fs.Write($bytes, 0, $bytes.Length)
    $fs.Flush($true)
} finally { $fs.Close() }
Dismount-DiskImage -ImagePath "$pwd\wrapper.vhdx" | Out-Null

# --- Mount + capture diagnostics --------------------------------------
Write-Host "[3/6] Mounting + capturing diagnostics ..."
$lettersBefore = @((Get-Volume | Where-Object { $_.DriveLetter }).DriveLetter)
$vhd = Mount-DiskImage -ImagePath "$pwd\wrapper.vhdx" -PassThru
$letter = $null
for ($i = 0; $i -lt 10; $i++) {
    Start-Sleep -Seconds 1
    $lettersAfter = @((Get-Volume | Where-Object { $_.DriveLetter }).DriveLetter)
    $new = $lettersAfter | Where-Object { $_ -notin $lettersBefore }
    if ($new) { $letter = $new | Select-Object -First 1; break }
}
if (-not $letter) {
    $disk2 = Get-Disk -Number $vhd.Number
    $partition = Get-Partition -DiskNumber $disk2.Number |
        Where-Object { $_.Type -ne 'Reserved' } | Select-Object -First 1
    $used = (Get-Volume | ForEach-Object { $_.DriveLetter }) +
            (Get-PSDrive -PSProvider FileSystem | ForEach-Object { $_.Name })
    foreach ($c in [char[]](68..90)) {
        if ($c -notin $used) {
            try {
                Set-Partition -DiskNumber $disk2.Number `
                    -PartitionNumber $partition.PartitionNumber `
                    -NewDriveLetter $c -ErrorAction Stop
                $letter = "$c"; break
            } catch { }
        }
    }
}
if (-not $letter) {
    Get-Disk | Format-List | Out-File diag/get-disk-on-failure.txt
    Get-Volume | Format-List | Out-File diag/get-volume-on-failure.txt
    Dismount-DiskImage -ImagePath "$pwd\wrapper.vhdx" -ErrorAction SilentlyContinue | Out-Null
    throw "no drive letter assigned even with Set-Partition fallback"
}
Write-Host "  Mounted at ${letter}:"
Get-Disk | Format-List | Out-File diag/get-disk-on-mount.txt
Get-Volume | Format-List | Out-File diag/get-volume-on-mount.txt
Get-Partition -ErrorAction SilentlyContinue | Format-List | Out-File diag/get-partition-on-mount.txt

# Enumerate the freshly-formatted root for the win:enumerate leg of
# scenarios like mac-format-basic-256mib. -Force surfaces hidden system
# files; user-visible content should be empty on a clean format.
"with -Force:" | Out-File diag/enumerate-root.txt
Get-ChildItem -LiteralPath "${letter}:\" -Force -ErrorAction SilentlyContinue |
    Select-Object Mode, Length, Name | Format-Table -AutoSize |
    Out-File diag/enumerate-root.txt -Append
"" | Out-File diag/enumerate-root.txt -Append
"user-visible (no -Force):" | Out-File diag/enumerate-root.txt -Append
$visible = @(Get-ChildItem -LiteralPath "${letter}:\" -ErrorAction SilentlyContinue)
if ($visible.Count -eq 0) {
    "(empty)" | Out-File diag/enumerate-root.txt -Append
} else {
    $visible | Select-Object Mode, Length, Name | Format-Table -AutoSize |
        Out-File diag/enumerate-root.txt -Append
}
"user_visible_count=$($visible.Count)" | Tee-Object diag/enumerate-root-count.txt | Out-Null

# Event log (NTFS / Disk / partmgr).
try {
    Get-WinEvent -LogName 'System' -MaxEvents 100 -ErrorAction SilentlyContinue |
        Where-Object { $_.ProviderName -in 'Ntfs','Microsoft-Windows-Ntfs','Disk','Volsnap','partmgr' } |
        Select-Object TimeCreated, ProviderName, Id, LevelDisplayName, Message |
        Format-List | Out-File diag/eventlog-fs.txt
} catch { }

# --- Reference NTFS volume (Microsoft format.com) ---------------------
Write-Host "[4/6] Building reference NTFS via format.com ..."
qemu-img create -f vhdx -o subformat=fixed reference.vhdx "${WrapperSizeMb}M" |
    Out-File diag/qemu-create-reference.txt
fsutil sparse setflag reference.vhdx 0 | Out-Null
$refVhd = Mount-DiskImage -ImagePath "$pwd\reference.vhdx" -PassThru
Start-Sleep -Seconds 2
Initialize-Disk -Number $refVhd.Number -PartitionStyle GPT
Start-Sleep -Seconds 2
$refDisk = Get-Disk -Number $refVhd.Number
$refPart = New-Partition -DiskNumber $refVhd.Number -UseMaximumSize -AssignDriveLetter:$true
Start-Sleep -Seconds 3
$refPart = Get-Partition -DiskNumber $refVhd.Number |
    Where-Object { $_.Type -ne 'Reserved' } | Select-Object -First 1
$fmtArgs = @("$($refPart.DriveLetter):", "/FS:NTFS", "/Q", "/A:4096", "/L", "/V:CITESTREF", "/Y")
$fp = Start-Process -FilePath "format.com" -ArgumentList $fmtArgs `
    -NoNewWindow -PassThru -Wait `
    -RedirectStandardOutput diag/reference-format.txt
"format.com exit: $($fp.ExitCode)" | Out-File diag/reference-format-exit.txt

# Read reference boot + MFT records.
$rawRefPath = "\\.\PhysicalDrive$($refDisk.Number)"
$refsh = [System.IO.File]::Open($rawRefPath, [System.IO.FileMode]::Open,
    [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
try {
    $refsh.Seek($refPart.Offset, [System.IO.SeekOrigin]::Begin) | Out-Null
    $refBoot = New-Object byte[] 512
    $null = $refsh.Read($refBoot, 0, 512)
    $refBps = [System.BitConverter]::ToUInt16($refBoot, 0x0B)
    $refSpc = $refBoot[0x0D]
    $refClusterSize = [int]$refBps * [int]$refSpc
    $refMftLcn = [System.BitConverter]::ToUInt64($refBoot, 0x30)
    $refMftFileOff = [int64]$refMftLcn * [int64]$refClusterSize
    $refsh.Seek($refPart.Offset + $refMftFileOff, [System.IO.SeekOrigin]::Begin) | Out-Null
    $refMft = New-Object byte[] (64KB)
    $null = $refsh.Read($refMft, 0, 64KB)
    [System.IO.File]::WriteAllBytes("$pwd\diag\reference-boot.bin", $refBoot)
    [System.IO.File]::WriteAllBytes("$pwd\diag\reference-mft-16recs.bin", $refMft)
    "Reference NTFS BPB: bps=$refBps spc=$refSpc cluster=$refClusterSize mft_lcn=$refMftLcn mft_off=$refMftFileOff" |
        Tee-Object diag/reference-bpb.txt
} finally { $refsh.Close() }
Dismount-DiskImage -ImagePath "$pwd\reference.vhdx" | Out-Null

# Dump our MFT from nfs.img (already on disk).
$ourBoot = $bytes[0..511]
$ourMft = $bytes[16384..(16384+65535)]
[System.IO.File]::WriteAllBytes("$pwd\diag\ours-boot.bin", $ourBoot)
[System.IO.File]::WriteAllBytes("$pwd\diag\ours-mft-16recs.bin", $ourMft)

# --- chkdsk -----------------------------------------------------------
Write-Host "[5/6] chkdsk passes ..."
$proc1 = Start-Process -FilePath chkdsk.exe -ArgumentList "${letter}:" `
    -NoNewWindow -PassThru -Wait `
    -RedirectStandardOutput diag/chkdsk-readonly.txt `
    -RedirectStandardError diag/chkdsk-readonly-stderr.txt
"chkdsk ${letter}: exit: $($proc1.ExitCode)" | Tee-Object diag/chkdsk-readonly-exit.txt

$proc2 = Start-Process -FilePath chkdsk.exe -ArgumentList "${letter}:","/scan" `
    -NoNewWindow -PassThru -Wait `
    -RedirectStandardOutput diag/chkdsk-scan.txt `
    -RedirectStandardError diag/chkdsk-scan-stderr.txt
"chkdsk ${letter}: /scan exit: $($proc2.ExitCode)" | Tee-Object diag/chkdsk-scan-exit.txt

# --- Cleanup ----------------------------------------------------------
Write-Host "[6/6] Dismounting ..."
Dismount-DiskImage -ImagePath "$pwd\wrapper.vhdx" -ErrorAction SilentlyContinue | Out-Null

Write-Host ""
Write-Host "=== chkdsk ${letter}: (read-only) ==="
Get-Content diag/chkdsk-readonly.txt
Write-Host "=== chkdsk verdict: readonly=$($proc1.ExitCode) /scan=$($proc2.ExitCode) ==="
exit $proc2.ExitCode
