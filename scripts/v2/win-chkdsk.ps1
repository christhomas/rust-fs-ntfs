# scripts/v2/win-chkdsk.ps1 -- minimal win-side helper for v2 recipes.
#
# A v2 alternative to scripts/run-scenario.ps1's chkdsk lifecycle,
# trimmed to just what's needed when the v2 dispatcher invokes it
# per-step over SSH. The full v1 driver still exists; this script is
# the per-op replacement chain that retires it.
#
# Wraps a host-side .img (already shipped to the VM via the harness's
# built-in `ship-to-vm` op) into a temporary VHDX, mounts it on
# Windows, runs chkdsk against the resulting drive letter with the
# requested modes, dismounts, and cleans up.
#
# Args:
#   -ImagePath  Path on the VM to the .img file (typically
#               <vm.workdir>/<scenario.image>).
#   -Modes      Comma-separated list of chkdsk passes to run, in
#               order: readonly, /scan, /spotfix, /F, /F /scan.
#               Empty / absent => run readonly only (matches the
#               default `mac:format -> win:chkdsk` shape).
#   -Diag       Directory to write diag artefacts into:
#                 chkdsk-<mode>.txt        — chkdsk's stdout
#                 chkdsk-<mode>-exit.txt   — exit code marker
#                 mount-eventlog.txt       — Disk/Ntfs/partmgr events
#                 wrapper-create.txt       — qemu-img output
#
# Exit code:
#   0 if every chkdsk mode exited 0
#   1 otherwise; per-mode exit codes are in <Diag>/chkdsk-*-exit.txt
#
# Phase 1e replacement target: this script invokes `qemu-img` to wrap
# the .img into a VHDX — the same dependency that Phase 1e plans to
# replace with `am-img-vhd::create_fixed`. When that lands, the
# `qemu-img create` line below becomes a thin invocation of the
# Antimatter Studios VHD writer; the rest of the lifecycle (mount,
# initialize, dd, chkdsk) stays the same.

param(
    [Parameter(Mandatory=$true)] [string]$ImagePath,
    [string]$Modes = "readonly",
    [Parameter(Mandatory=$true)] [string]$Diag
)

$ErrorActionPreference = 'Stop'

# qemu-img is on the PATH via setup-windows-vm.ps1's package install.
$env:PATH = "C:\Program Files\Cloudbase Solutions\QEMU\bin;$env:PATH"

if (-not (Test-Path $ImagePath)) {
    Write-Error "image not found on VM: $ImagePath"
    exit 2
}

New-Item -ItemType Directory -Path $Diag -Force | Out-Null

# Sized just larger than the .img so the GPT slack fits.
$rawSize     = (Get-Item $ImagePath).Length
$rawSizeMb   = [int][Math]::Ceiling($rawSize / 1MB)
$wrapperMb   = $rawSizeMb + 64
$Vhdx        = [System.IO.Path]::ChangeExtension($ImagePath, ".vhdx")

# Ensure a clean slate — any prior wrapper for this scenario is
# torn down before we start.
foreach ($v in @($Vhdx)) {
    try {
        Get-DiskImage -ImagePath $v -EA SilentlyContinue |
            Where-Object Attached |
            Dismount-DiskImage -EA SilentlyContinue | Out-Null
    } catch { }
}
Remove-Item -LiteralPath $Vhdx -Force -EA SilentlyContinue

$startTime = Get-Date

try {
    # ── Wrap .img into a VHDX (Phase 1e replacement target) ───────
    & qemu-img create -f vhdx -o subformat=fixed $Vhdx "${wrapperMb}M" *> "$Diag\wrapper-create.txt"
    if ($LASTEXITCODE -ne 0) {
        throw "qemu-img create failed exit=$LASTEXITCODE (see wrapper-create.txt)"
    }
    fsutil sparse setflag $Vhdx 0 | Out-Null

    # ── Mount + initialise ────────────────────────────────────────
    $vhd = Mount-DiskImage -ImagePath $Vhdx -PassThru
    Start-Sleep -Seconds 2
    Initialize-Disk -Number $vhd.Number -PartitionStyle GPT
    Start-Sleep -Seconds 2
    $disk = Get-Disk -Number $vhd.Number
    $part = New-Partition -DiskNumber $vhd.Number -UseMaximumSize -AssignDriveLetter:$false
    if ($part.Size -lt $rawSize) {
        throw "partition smaller than raw image ($($part.Size) < $rawSize)"
    }

    # Write the .img bytes into the partition's offset on the raw
    # disk. Mirrors what scripts/run-scenario.ps1 does in v1.
    $rawPath = "\\.\PhysicalDrive$($disk.Number)"
    $imgBytes = [System.IO.File]::ReadAllBytes($ImagePath)
    $fs = [System.IO.File]::Open($rawPath, [System.IO.FileMode]::Open,
        [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::ReadWrite)
    try {
        $fs.Seek($part.Offset, [System.IO.SeekOrigin]::Begin) | Out-Null
        $fs.Write($imgBytes, 0, $imgBytes.Length)
        $fs.Flush($true)
    } finally { $fs.Close() }

    # Dismount + remount so ntfs.sys re-recognises the populated
    # partition and assigns a drive letter.
    Dismount-DiskImage -ImagePath $Vhdx | Out-Null
    Start-Sleep -Seconds 1
    $lettersBefore = @((Get-Volume | Where-Object { $_.DriveLetter }).DriveLetter)
    $vhd = Mount-DiskImage -ImagePath $Vhdx -PassThru
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
    if (-not $letter) { throw "no drive letter assigned" }

    # ── chkdsk passes ─────────────────────────────────────────────
    #
    # Pass/fail rules — match v1's `Clean` VerdictShape (matrix.rs:999):
    #
    #   readonly mode:  must exit 0
    #   /scan mode:     0, 11, 13 are all "ok"
    #     - 0  = clean
    #     - 11 = frs.cxx 60f ceiling (known v1 technical debt — not real
    #            corruption; matrix.rs's existing verdict tolerates it)
    #     - 13 = VSS / shadow-copy infra error on tiny volumes; same
    #            class of "infrastructure flake, not corruption"
    #
    # Future RepairOk / RepairRequired shapes will need a `-VerdictShape`
    # parameter; not implemented in this slice — every scenario using
    # this script today defaults to Clean.
    $rawExits  = @{}      # mode -> exit code (for diag inspection)
    $passed    = $true
    foreach ($mode in $Modes.Split(',') | ForEach-Object { $_.Trim() } | Where-Object { $_ }) {
        $modeFile = $mode -replace '[/\\ ]', '-'
        $log = "$Diag\chkdsk-$modeFile.txt"
        $exitFile = "$Diag\chkdsk-$modeFile-exit.txt"
        $args = @("${letter}:")
        if ($mode -ne "readonly") {
            $args += $mode -split ' '
        }
        $proc = Start-Process -FilePath chkdsk -ArgumentList $args -NoNewWindow -PassThru -Wait -RedirectStandardOutput $log
        "$($proc.ExitCode)" | Out-File $exitFile -Encoding ASCII
        $rawExits[$mode] = $proc.ExitCode

        # Apply Clean-shape verdict per-mode.
        if ($mode -eq 'readonly') {
            if ($proc.ExitCode -ne 0) { $passed = $false }
        } else {
            # /scan and other modes: accept 0/11/13 as Clean-shape "ok".
            if ($proc.ExitCode -ne 0 -and $proc.ExitCode -ne 11 -and $proc.ExitCode -ne 13) {
                $passed = $false
            }
        }
    }

    # Emit a verdict summary file so a triage agent doesn't have to
    # parse all the chkdsk-*-exit.txt files individually.
    @{
        passed = $passed
        verdict_shape = "clean"
        exits = $rawExits
    } | ConvertTo-Json -Compress | Out-File "$Diag\verdict.json" -Encoding ASCII

    # NTFS / Disk / partmgr events fired during this run.
    try {
        Get-WinEvent -LogName 'System' -EA SilentlyContinue |
            Where-Object {
                $_.TimeCreated -ge $startTime -and
                $_.ProviderName -in 'Ntfs','Microsoft-Windows-Ntfs','Disk','Volsnap','partmgr'
            } |
            Select-Object TimeCreated, ProviderName, Id, LevelDisplayName, Message |
            Format-List | Out-File "$Diag\mount-eventlog.txt"
    } catch { }

    if ($passed) { exit 0 } else { exit 1 }

} finally {
    # Tear down the VHDX wrapper. Leftover wrappers from a crashed run
    # would block a subsequent Mount-DiskImage with a drive-letter
    # collision, plus they consume real bytes on the C: drive — not
    # negligible for the 1GiB+ scenarios.
    foreach ($v in @($Vhdx)) {
        try {
            Get-DiskImage -ImagePath $v -EA SilentlyContinue |
                Where-Object Attached |
                Dismount-DiskImage -EA SilentlyContinue | Out-Null
        } catch { }
    }
    Remove-Item -LiteralPath $Vhdx -Force -EA SilentlyContinue

    # Drop the shipped .img too. The harness's `ship-to-vm` op puts it
    # here at scenario start; without explicit cleanup we accumulate
    # one image-sized file per scenario, which fills the C: drive
    # within a single matrix run (a 16 GiB volume scenario alone is
    # most of a 64 GiB VM disk). The Mac side keeps its own copy if
    # post-mortem inspection is needed; the .vhdx wrapper cleanup
    # already takes the actual disk-image artefacts with it.
    Remove-Item -LiteralPath $ImagePath -Force -EA SilentlyContinue
}
