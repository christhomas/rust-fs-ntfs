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

    # Take the disk offline before opening the raw `\\.\PhysicalDriveN`
    # handle. Without this, chunked writes to the raw handle return
    # `Access to the path is denied.` once the volume layer auto-detects
    # the new partition and locks it — v1's run-scenario.ps1 documents
    # the same failure mode (lines 115-122) and explicitly skips
    # >2 GiB scenarios to avoid it. Setting the disk offline tells the
    # Volume Manager to release any volume-level holds; raw block-layer
    # writes via `\\.\PhysicalDriveN` still work while offline. The disk
    # is brought back online in this block's `finally` so it's restored
    # even if the streaming throws — otherwise an exception here would
    # leave the disk offline and the outer cleanup's `Dismount-DiskImage`
    # behaviour against an offline VHD isn't guaranteed across Windows
    # versions.
    Set-Disk -Number $disk.Number -IsOffline $true
    $writeFailed = $false
    try {
        # Stream the .img bytes into the partition's offset on the raw
        # disk. v1's `[IO.File]::ReadAllBytes` capped at ~2 GiB (.NET
        # `byte[]` length is Int32); chunked Read + Write avoids the cap.
        $rawPath = "\\.\PhysicalDrive$($disk.Number)"
        # FileShare.Read on the source matches `[IO.File]::ReadAllBytes`'s
        # internal share — denies write-sharing during the copy so a
        # concurrent scp retry can't produce a torn image.
        $src = [System.IO.File]::Open($ImagePath, [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::Read, [System.IO.FileShare]::Read)
        try {
            $dst = [System.IO.File]::Open($rawPath, [System.IO.FileMode]::Open,
                [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::ReadWrite)
            try {
                $dst.Seek($part.Offset, [System.IO.SeekOrigin]::Begin) | Out-Null
                # Raw writes require offset + length to be multiples of the
                # physical sector size; pad the trailing chunk's `[n, aligned)`
                # window with zeros (Read rewrote `[0, n)` so only the pad
                # region could be stale).
                $sectorSize = $disk.PhysicalSectorSize
                $bufSize = 16MB
                $buf = New-Object byte[] $bufSize
                try {
                    while ($true) {
                        $n = $src.Read($buf, 0, $bufSize)
                        if ($n -le 0) { break }
                        $aligned = [int][Math]::Ceiling($n / $sectorSize) * $sectorSize
                        if ($aligned -gt $n) {
                            [Array]::Clear($buf, $n, $aligned - $n)
                        }
                        $dst.Write($buf, 0, $aligned)
                    }
                    $dst.Flush($true)
                } catch {
                    $writeFailed = $true
                    throw
                }
            } finally { $dst.Close() }
        } finally { $src.Close() }
    } finally {
        # Bring the disk back online so ntfs.sys can mount the populated
        # partition for chkdsk. If the streaming write itself failed,
        # silence any restore error so the original write exception is
        # what propagates. Otherwise let the restore fail loudly — a
        # silent restore failure here would surface later as a
        # cryptic `Dismount-DiskImage` / drive-letter error.
        if ($writeFailed) {
            Set-Disk -Number $disk.Number -IsOffline $false -EA SilentlyContinue
        } else {
            Set-Disk -Number $disk.Number -IsOffline $false -EA Stop
        }
    }

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
    #
    # Best-effort delete: don't crash the script if the file's
    # somehow held by a process that didn't release. But verify
    # afterward and emit a warning + per-scenario diag entry —
    # silently suppressing the failure is exactly what would let the
    # leak recur unnoticed, which defeats the whole point of this
    # cleanup block.
    Remove-Item -LiteralPath $ImagePath -Force -EA SilentlyContinue
    if (Test-Path -LiteralPath $ImagePath) {
        "cleanup_failed image_path=$ImagePath" |
            Out-File "$Diag\cleanup-warnings.txt" -Append -Encoding ASCII
        Write-Warning "Failed to remove shipped image: $ImagePath (still on disk after Remove-Item; see cleanup-warnings.txt)"
    }
}
