# scripts/v2/_lib.ps1 -- shared helpers for v2 win-side op scripts.
#
# Dot-source this from win-chkdsk.ps1, win-enumerate.ps1 and any future
# win-* helper. Functions are deliberately small and stateless: each
# returns the bits the caller needs, and the caller controls the
# try/finally lifecycle.
#
# Two execution shapes share these helpers:
#   - "first op for a scenario": .img is on the VM (shipped), no .vhd
#     yet. Init-VhdFromImg creates the wrapper + streams the .img bytes
#     into the partition, leaves the volume ready to mount.
#   - "follow-on op": .vhd already exists on disk from a prior op (the
#     prior op was invoked with KeepImage=true). Init-VhdFromImg sees
#     the existing .vhd and skips the create+stream phase.
#
# Mount-VhdAndGetLetter does the dismount/remount + letter detection
# dance that ntfs.sys requires after a fresh raw write — used by
# every op after the volume's bytes are in place.

# vhd_tool is on the PATH via setup-windows-vm.ps1 (cargo install
# --bin vhd_tool puts it in ~/.cargo/bin which rustup adds to PATH).
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"

# ── Drive-letter mutex ────────────────────────────────────────────────────────
# Serialises the "snapshot lettersBefore → Mount-DiskImage → detect new letter"
# sequence in Mount-VhdAndGetLetter. Without this, two concurrent op scripts
# can both capture the same lettersBefore set and then both claim the same
# newly-assigned letter as "theirs", causing Format-Volume / chkdsk / etc. to
# target the wrong drive letter or fail with "No MSFT_Volume objects found".
#
# FileMode.CreateNew is atomic on Windows NTFS — exactly one caller wins the
# race to create the file; all others get IOException and retry.

$script:DriveLockPath = "$env:TEMP\vhd-drive-lock"

function Acquire-DriveLock {
    $deadline = [DateTime]::UtcNow.AddSeconds(180)
    while ($true) {
        try {
            $fs = [System.IO.File]::Open(
                $script:DriveLockPath,
                [System.IO.FileMode]::CreateNew,
                [System.IO.FileAccess]::Write,
                [System.IO.FileShare]::None)
            $fs.Close()
            return
        } catch [System.IO.IOException] {
            if ([DateTime]::UtcNow -ge $deadline) {
                throw "Acquire-DriveLock: timed out after 180s waiting for $($script:DriveLockPath)"
            }
            Start-Sleep -Milliseconds 250
        }
    }
}

function Release-DriveLock {
    Remove-Item -LiteralPath $script:DriveLockPath -Force -EA SilentlyContinue
}

function Get-VhdPathFor {
    param([Parameter(Mandatory=$true)] [string]$ImagePath)
    return [System.IO.Path]::ChangeExtension($ImagePath, ".vhd")
}

# Wrap an .img into a fixed VHD, mount, GPT-init, partition, and stream
# the .img bytes into the partition's offset. Returns @{ Vhd; Disk; }
# (the caller passes Vhd through to Mount-VhdAndGetLetter and Disk
# isn't used after this — kept for diag/debugging).
#
# Idempotent: if the .vhd already exists on disk (because a prior op
# in the same scenario ran with KeepImage=true), the create+stream
# phase is skipped and we just return the existing Vhd path. The
# caller still needs to call Mount-VhdAndGetLetter to get a letter.
function Initialize-VhdFromImg {
    param(
        [Parameter(Mandatory=$true)] [string]$ImagePath,
        [Parameter(Mandatory=$true)] [string]$Diag
    )

    if (-not (Test-Path $ImagePath)) {
        throw "image not found on VM: $ImagePath"
    }

    $Vhd = Get-VhdPathFor -ImagePath $ImagePath

    # Belt-and-braces: tear down any orphaned mount of this Vhd path
    # before we look at the file. A crashed prior run could leave the
    # VHD both on disk *and* attached — the reuse fast path below
    # would then return early and the caller's Mount-VhdAndGetLetter
    # would fail trying to remount an already-attached image. Run this
    # before the existence check so both paths self-heal.
    try {
        Get-DiskImage -ImagePath $Vhd -EA SilentlyContinue |
            Where-Object Attached |
            Dismount-DiskImage -EA SilentlyContinue | Out-Null
    } catch { }

    # Already-streamed VHD from a prior op — fast path: skip the
    # vhd_tool + stream phase, just return the path.
    #
    # Stale-VHD detection: if the .img is *newer* than the .vhd, a
    # mac-side op (or a re-ship-to-vm) modified the source bytes after
    # the VHD was last streamed. Reusing the VHD would mount the
    # OLD bytes — the round-trip win-format scenarios depend on this
    # detection (`win:format -> ship-to-host -> mac:write -> ship-to-vm
    # -> win:chkdsk`: the second win-* op must see the post-mac-write
    # bytes, not the pre-mac-write ones from the prior win-format).
    # Wipe the stale VHD and fall through to rebuild.
    if (Test-Path $Vhd) {
        $imgWriteTime = (Get-Item $ImagePath).LastWriteTimeUtc
        $vhdWriteTime = (Get-Item $Vhd).LastWriteTimeUtc
        if ($imgWriteTime -le $vhdWriteTime) {
            return @{ Vhd = $Vhd }
        }
        # .img is newer — VHD is stale. Delete and rebuild.
        Remove-Item -LiteralPath $Vhd -Force -EA SilentlyContinue
        if (Test-Path -LiteralPath $Vhd) {
            throw "Stale VHD could not be removed: $Vhd (img mtime $imgWriteTime > vhd mtime $vhdWriteTime)"
        }
    }

    # Sized just larger than the .img so the GPT slack fits. vhd_tool's
    # create-fixed takes raw bytes (no MB suffix), so multiply by 1MB
    # explicitly before passing.
    $rawSize          = (Get-Item $ImagePath).Length
    $rawSizeMb        = [int][Math]::Ceiling($rawSize / 1MB)
    $wrapperMb        = $rawSizeMb + 64
    $wrapperSizeBytes = [int64]$wrapperMb * 1MB

    & vhd_tool create-fixed $Vhd $wrapperSizeBytes *> "$Diag\wrapper-create.txt"
    if ($LASTEXITCODE -ne 0) {
        throw "vhd_tool create-fixed failed exit=$LASTEXITCODE (see wrapper-create.txt)"
    }
    fsutil sparse setflag $Vhd 0 | Out-Null

    # PowerShell variable names are case-insensitive — assigning the
    # Mount-DiskImage CimInstance to `$vhd` would silently overwrite
    # the `$Vhd` path string, breaking the `Dismount-DiskImage -ImagePath
    # $Vhd` call below and the returned hashtable's `Vhd` field. Use
    # a distinct name for the CimInstance.
    $mountedImg = Mount-DiskImage -ImagePath $Vhd -PassThru
    Start-Sleep -Seconds 2
    Initialize-Disk -Number $mountedImg.Number -PartitionStyle GPT
    Start-Sleep -Seconds 2
    $disk = Get-Disk -Number $mountedImg.Number
    $part = New-Partition -DiskNumber $mountedImg.Number -UseMaximumSize -AssignDriveLetter:$false
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

    # Dismount the just-streamed VHD. The caller's Mount-VhdAndGetLetter
    # remounts it so ntfs.sys re-recognises the populated partition and
    # assigns a drive letter.
    Dismount-DiskImage -ImagePath $Vhd | Out-Null

    return @{ Vhd = $Vhd; Disk = $disk }
}

# Mount the VHD, wait for ntfs.sys to assign a drive letter (with a
# manual Set-Partition fallback if auto-assignment doesn't happen
# within 10s). Returns the bare letter (e.g. "E"). Throws if no letter
# can be obtained.
function Mount-VhdAndGetLetter {
    param([Parameter(Mandatory=$true)] [string]$Vhd)

    # Settle pause outside the lock: let any prior Dismount-DiskImage complete
    # before we enter the critical section. The old monolithic win-chkdsk.ps1
    # had this between dismount and remount; the lib refactor split those
    # across functions and dropped it. Kept here so ntfs.sys has a chance to
    # finish tearing down before we snapshot the letter table.
    Start-Sleep -Seconds 1

    # Hold the drive-letter mutex for the entire snapshot → mount → detect
    # sequence. Without it, two concurrent scripts both capture the same
    # lettersBefore set and claim the same newly-assigned letter.
    Acquire-DriveLock
    try {
        $lettersBefore = @((Get-Volume | Where-Object { $_.DriveLetter }).DriveLetter)
        # Distinct name for the CimInstance — avoids the case-insensitive
        # collision with the `$Vhd` path-string param. Benign here (the
        # path isn't used again after this line), but consistent with
        # Initialize-VhdFromImg.
        $mountedImg = Mount-DiskImage -ImagePath $Vhd -PassThru
        $letter = $null
        for ($i = 0; $i -lt 10; $i++) {
            Start-Sleep -Seconds 1
            $lettersAfter = @((Get-Volume | Where-Object { $_.DriveLetter }).DriveLetter)
            $new = $lettersAfter | Where-Object { $_ -notin $lettersBefore }
            if ($new) { $letter = $new | Select-Object -First 1; break }
        }
        if (-not $letter) {
            $disk2 = Get-Disk -Number $mountedImg.Number
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
    } finally {
        Release-DriveLock
    }
}

# Read the partition contents out of the mounted VHD's raw device
# and write them back over the .img file. The inverse of the byte
# stream `Initialize-VhdFromImg` does on the way in — without this
# the .img on the VM stays at whatever bytes it had at ship-to-vm
# time (zeros for `win-format-*` scenarios), even though the VHD
# wrapper now holds the win-* op's writes. The result: `ship-to-host`
# would pull back a stale .img that doesn't reflect anything Windows
# did, and the follow-on mac-* ops (mac-enumerate, mac-rm, …) all
# fail on the zero-filled buffer.
#
# Symmetric with `Initialize-VhdFromImg`'s write loop: takes the disk
# offline to break the volume-layer file lock, opens
# `\\.\PhysicalDriveN` raw, seeks to the partition offset, reads
# `(Get-Item .img).Length` bytes back into the .img, sets the disk
# online again. Sector-aligned reads with a buffered loop.
#
# Idempotent for read-only ops (chkdsk readonly etc.) — the partition
# bytes match what was streamed in, so the .img stays byte-identical.
# We accept the wasted IO for the read-only case to keep the
# Dismount-VhdAndCleanup contract uniform: "after this returns, the
# .img file reflects the post-op state of the volume."
function Sync-VhdToImg {
    param(
        [Parameter(Mandatory=$true)] [string]$Vhd,
        [Parameter(Mandatory=$true)] [string]$ImagePath
    )

    # The VHD must still be attached (mounted as a disk image) for
    # the raw \\.\PhysicalDriveN path to resolve. Callers should run
    # this before `Dismount-VhdAndCleanup`.
    $mountedImg = Get-DiskImage -ImagePath $Vhd -EA SilentlyContinue
    if (-not $mountedImg -or -not $mountedImg.Attached) {
        throw "Sync-VhdToImg: VHD not attached: $Vhd"
    }
    $disk = Get-Disk -Number $mountedImg.Number
    $part = Get-Partition -DiskNumber $disk.Number |
        Where-Object { $_.Type -ne 'Reserved' } | Select-Object -First 1
    if (-not $part) {
        throw "Sync-VhdToImg: no data partition on disk $($disk.Number)"
    }
    $imgSize = (Get-Item $ImagePath).Length
    if ($part.Size -lt $imgSize) {
        throw ("Sync-VhdToImg: partition smaller than .img " +
               "(part=$($part.Size) img=$imgSize)")
    }

    # Same offline/raw-handle/online dance as Initialize-VhdFromImg's
    # write phase. Without this Windows holds an exclusive volume
    # handle that makes the raw open fail with "Access is denied."
    Set-Disk -Number $disk.Number -IsOffline $true
    $readFailed = $false
    try {
        $rawPath = "\\.\PhysicalDrive$($disk.Number)"
        $src = [System.IO.File]::Open($rawPath, [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
        try {
            try {
                $dst = [System.IO.File]::Open($ImagePath, [System.IO.FileMode]::Open,
                    [System.IO.FileAccess]::Write, [System.IO.FileShare]::Read)
                try {
                    $src.Seek($part.Offset, [System.IO.SeekOrigin]::Begin) | Out-Null
                    $sectorSize = $disk.PhysicalSectorSize
                    $bufSize = 16MB
                    $buf = New-Object byte[] $bufSize
                    $remaining = $imgSize
                    while ($remaining -gt 0) {
                        # Raw device reads must be sector-aligned; the
                        # tail chunk reads more than `remaining` and we
                        # only persist the first `remaining` bytes to
                        # the .img.
                        $request = [int][Math]::Min($bufSize, [Math]::Ceiling($remaining / $sectorSize) * $sectorSize)
                        $n = $src.Read($buf, 0, $request)
                        if ($n -le 0) { break }
                        $writeLen = [int][Math]::Min($n, $remaining)
                        $dst.Write($buf, 0, $writeLen)
                        $remaining -= $writeLen
                    }
                    if ($remaining -gt 0) {
                        # A 0-byte raw read with bytes still expected is a
                        # truncation, not a normal EOF. Fail loudly so the
                        # caller doesn't ship a partially-synced .img.
                        throw "Sync-VhdToImg: short read from raw device; $remaining bytes not copied"
                    }
                    $dst.Flush($true)
                } finally { $dst.Close() }
            } catch {
                $readFailed = $true
                throw
            }
        } finally { $src.Close() }
    } finally {
        if ($readFailed) {
            Set-Disk -Number $disk.Number -IsOffline $false -EA SilentlyContinue
        } else {
            Set-Disk -Number $disk.Number -IsOffline $false -EA Stop
        }
    }
}

# Tear down a VHD wrapper (best-effort dismount + remove the file).
# `KeepImage=$true` leaves the source .img and the .vhd in place so a
# follow-on op in the same scenario can mount them again. The final op
# in the scenario should use KeepImage=$false (or call Remove-ScenarioImage
# explicitly) to avoid leaving GiB-sized artefacts on the VM.
#
# **`SyncBack`** (default `$true`): before dismount, copy the
# partition bytes out of `\\.\PhysicalDriveN` back to the `.img` file
# so the `.img` reflects the post-op state of the volume. This is the
# fix for `win-format-*` scenarios where the win-* op's writes land
# in the VHD wrapper and never propagate to the `.img` — see
# `Sync-VhdToImg` for the full rationale. Callers that are sure the
# op was strictly read-only (no volume metadata updates, no
# last-access timestamps) can pass `$false` to skip the sync IO, but
# the default is safe-and-redundant rather than fast-and-wrong.
function Dismount-VhdAndCleanup {
    param(
        [Parameter(Mandatory=$true)] [string]$Vhd,
        [Parameter(Mandatory=$true)] [string]$ImagePath,
        [bool]$KeepImage = $false,
        [bool]$SyncBack = $true
    )

    if ($SyncBack) {
        try {
            Sync-VhdToImg -Vhd $Vhd -ImagePath $ImagePath
        } catch {
            Write-Warning "Sync-VhdToImg failed (continuing with dismount): $_"
        }
    }

    try {
        Get-DiskImage -ImagePath $Vhd -EA SilentlyContinue |
            Where-Object Attached |
            Dismount-DiskImage -EA SilentlyContinue | Out-Null
    } catch { }

    if ($KeepImage) { return }

    # Best-effort delete with explicit verification on both files.
    # Silently swallowing a cleanup failure was the bug PR #9 fixed for
    # the .img; the .vhd needs the same treatment because
    # Initialize-VhdFromImg uses .vhd existence as its idempotency
    # signal — a silently-failed wrapper cleanup would cause the next
    # scenario to skip the create+stream phase and remount stale data.
    Remove-Item -LiteralPath $Vhd -Force -EA SilentlyContinue
    if (Test-Path -LiteralPath $Vhd) {
        Write-Warning "Failed to remove VHD wrapper: $Vhd"
    }
    Remove-Item -LiteralPath $ImagePath -Force -EA SilentlyContinue
    if (Test-Path -LiteralPath $ImagePath) {
        Write-Warning "Failed to remove shipped image: $ImagePath"
    }
}
