# scripts/v2/_lib.ps1 -- shared helpers for v2 win-side op scripts.
#
# Dot-source this from win-chkdsk.ps1, win-enumerate.ps1 and any future
# win-* helper. Functions are deliberately small and stateless: each
# returns the bits the caller needs, and the caller controls the
# try/finally lifecycle.
#
# Two execution shapes share these helpers:
#   - "first op for a scenario": .img is on the VM (shipped), no .vhdx
#     yet. Init-VhdxFromImg creates the wrapper + streams the .img bytes
#     into the partition, leaves the volume ready to mount.
#   - "follow-on op": .vhdx already exists on disk from a prior op (the
#     prior op was invoked with KeepImage=true). Init-VhdxFromImg sees
#     the existing .vhdx and skips the create+stream phase.
#
# Mount-VhdxAndGetLetter does the dismount/remount + letter detection
# dance that ntfs.sys requires after a fresh raw write — used by
# every op after the volume's bytes are in place.

# qemu-img is on the PATH via setup-windows-vm.ps1's package install.
$env:PATH = "C:\Program Files\Cloudbase Solutions\QEMU\bin;$env:PATH"

function Get-VhdxPathFor {
    param([Parameter(Mandatory=$true)] [string]$ImagePath)
    return [System.IO.Path]::ChangeExtension($ImagePath, ".vhdx")
}

# Wrap an .img into a fixed VHDX, mount, GPT-init, partition, and stream
# the .img bytes into the partition's offset. Returns @{ Vhdx; Disk; }
# (the caller passes Vhdx through to Mount-VhdxAndGetLetter and Disk
# isn't used after this — kept for diag/debugging).
#
# Idempotent: if the .vhdx already exists on disk (because a prior op
# in the same scenario ran with KeepImage=true), the create+stream
# phase is skipped and we just return the existing Vhdx path. The
# caller still needs to call Mount-VhdxAndGetLetter to get a letter.
function Initialize-VhdxFromImg {
    param(
        [Parameter(Mandatory=$true)] [string]$ImagePath,
        [Parameter(Mandatory=$true)] [string]$Diag
    )

    if (-not (Test-Path $ImagePath)) {
        throw "image not found on VM: $ImagePath"
    }

    $Vhdx = Get-VhdxPathFor -ImagePath $ImagePath

    # Belt-and-braces: tear down any orphaned mount of this Vhdx path
    # before we look at the file. A crashed prior run could leave the
    # VHDX both on disk *and* attached — the reuse fast path below
    # would then return early and the caller's Mount-VhdxAndGetLetter
    # would fail trying to remount an already-attached image. Run this
    # before the existence check so both paths self-heal.
    try {
        Get-DiskImage -ImagePath $Vhdx -EA SilentlyContinue |
            Where-Object Attached |
            Dismount-DiskImage -EA SilentlyContinue | Out-Null
    } catch { }

    # Already-streamed VHDX from a prior op — fast path: skip the
    # qemu-img + stream phase, just return the path.
    #
    # Stale-VHDX detection: if the .img is *newer* than the .vhdx, a
    # mac-side op (or a re-ship-to-vm) modified the source bytes after
    # the VHDX was last streamed. Reusing the VHDX would mount the
    # OLD bytes — the round-trip win-format scenarios depend on this
    # detection (`win:format -> ship-to-host -> mac:write -> ship-to-vm
    # -> win:chkdsk`: the second win-* op must see the post-mac-write
    # bytes, not the pre-mac-write ones from the prior win-format).
    # Wipe the stale VHDX and fall through to rebuild.
    if (Test-Path $Vhdx) {
        $imgWriteTime = (Get-Item $ImagePath).LastWriteTimeUtc
        $vhdxWriteTime = (Get-Item $Vhdx).LastWriteTimeUtc
        if ($imgWriteTime -le $vhdxWriteTime) {
            return @{ Vhdx = $Vhdx }
        }
        # .img is newer — VHDX is stale. Delete and rebuild.
        Remove-Item -LiteralPath $Vhdx -Force -EA SilentlyContinue
        if (Test-Path -LiteralPath $Vhdx) {
            throw "Stale VHDX could not be removed: $Vhdx (img mtime $imgWriteTime > vhdx mtime $vhdxWriteTime)"
        }
    }

    # Sized just larger than the .img so the GPT slack fits.
    $rawSize     = (Get-Item $ImagePath).Length
    $rawSizeMb   = [int][Math]::Ceiling($rawSize / 1MB)
    $wrapperMb   = $rawSizeMb + 64

    & qemu-img create -f vhdx -o subformat=fixed $Vhdx "${wrapperMb}M" *> "$Diag\wrapper-create.txt"
    if ($LASTEXITCODE -ne 0) {
        throw "qemu-img create failed exit=$LASTEXITCODE (see wrapper-create.txt)"
    }
    fsutil sparse setflag $Vhdx 0 | Out-Null

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
    # handle — the volume layer otherwise auto-detects the new partition
    # and locks it, which makes chunked writes return "Access to the
    # path is denied." See PR #14 for the full story.
    Set-Disk -Number $disk.Number -IsOffline $true
    $writeFailed = $false
    try {
        $rawPath = "\\.\PhysicalDrive$($disk.Number)"
        $src = [System.IO.File]::Open($ImagePath, [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::Read, [System.IO.FileShare]::Read)
        try {
            try {
                $dst = [System.IO.File]::Open($rawPath, [System.IO.FileMode]::Open,
                    [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::ReadWrite)
                try {
                    $dst.Seek($part.Offset, [System.IO.SeekOrigin]::Begin) | Out-Null
                    # Pad the trailing chunk's `[n, aligned)` window with
                    # zeros so each Write is a multiple of the physical
                    # sector size; Read rewrote `[0, n)` already.
                    $sectorSize = $disk.PhysicalSectorSize
                    $bufSize = 16MB
                    $buf = New-Object byte[] $bufSize
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
                } finally { $dst.Close() }
            } catch {
                $writeFailed = $true
                throw
            }
        } finally { $src.Close() }
    } finally {
        if ($writeFailed) {
            Set-Disk -Number $disk.Number -IsOffline $false -EA SilentlyContinue
        } else {
            Set-Disk -Number $disk.Number -IsOffline $false -EA Stop
        }
    }

    # Dismount the just-streamed VHDX. The caller's Mount-VhdxAndGetLetter
    # remounts it so ntfs.sys re-recognises the populated partition and
    # assigns a drive letter.
    Dismount-DiskImage -ImagePath $Vhdx | Out-Null

    return @{ Vhdx = $Vhdx; Disk = $disk }
}

# Mount the VHDX, wait for ntfs.sys to assign a drive letter (with a
# manual Set-Partition fallback if auto-assignment doesn't happen
# within 10s). Returns the bare letter (e.g. "E"). Throws if no letter
# can be obtained.
function Mount-VhdxAndGetLetter {
    param([Parameter(Mandatory=$true)] [string]$Vhdx)

    # Brief pause so a prior Dismount-DiskImage (either from
    # Initialize-VhdxFromImg's tail or a prior op's
    # Dismount-VhdxAndCleanup) has a chance to fully settle before
    # ntfs.sys is asked to recognise the volume again. The old monolithic
    # win-chkdsk.ps1 had this `Start-Sleep -Seconds 1` between dismount
    # and remount; the lib refactor split those across functions and
    # dropped it. Removing it works on the dev VM but Windows' VHD stack
    # has been known to race the dismount completion on slower hosts.
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
    return "$letter"
}

# Tear down a VHDX wrapper (best-effort dismount + remove the file).
# `KeepImage=$true` leaves the source .img and the .vhdx in place so a
# follow-on op in the same scenario can mount them again. The final op
# in the scenario should use KeepImage=$false (or call Remove-ScenarioImage
# explicitly) to avoid leaving GiB-sized artefacts on the VM.
function Dismount-VhdxAndCleanup {
    param(
        [Parameter(Mandatory=$true)] [string]$Vhdx,
        [Parameter(Mandatory=$true)] [string]$ImagePath,
        [bool]$KeepImage = $false
    )

    try {
        Get-DiskImage -ImagePath $Vhdx -EA SilentlyContinue |
            Where-Object Attached |
            Dismount-DiskImage -EA SilentlyContinue | Out-Null
    } catch { }

    if ($KeepImage) { return }

    # Best-effort delete with explicit verification on both files.
    # Silently swallowing a cleanup failure was the bug PR #9 fixed for
    # the .img; the .vhdx needs the same treatment because
    # Initialize-VhdxFromImg uses .vhdx existence as its idempotency
    # signal — a silently-failed wrapper cleanup would cause the next
    # scenario to skip the create+stream phase and remount stale data.
    Remove-Item -LiteralPath $Vhdx -Force -EA SilentlyContinue
    if (Test-Path -LiteralPath $Vhdx) {
        Write-Warning "Failed to remove VHDX wrapper: $Vhdx"
    }
    Remove-Item -LiteralPath $ImagePath -Force -EA SilentlyContinue
    if (Test-Path -LiteralPath $ImagePath) {
        Write-Warning "Failed to remove shipped image: $ImagePath"
    }
}
