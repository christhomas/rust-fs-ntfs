# ci-make-image.ps1 -- generate the raw NTFS volume and wrap it in a
# GPT-partitioned VHD, with the byte-level diagnostics that say which
# layer produced a bad boot sector.
#
# EXTRACTED FROM ci.yml SO THE JOB LOG STOPS CARRYING IT. GitHub echoes
# every `run:` block into the log inside its ##[group]: this body was 138
# of the 1,046 lines the Windows job printed (#290). As a file it is one
# line there, and the same script is shared with release.yml instead of
# living twice.
#
# Writes diag/*.txt (uploaded as the ntfs-windows-diag artifact) and
# leaves nfs.img + nfs.vhd in the working directory for the mount step.
$ErrorActionPreference = "Stop"

# Generate the raw NTFS volume bytes via our binary.
# 256 MiB: chkdsk's `/scan` mode requires shadow-copy storage
# space on the volume. On a 64 MiB volume it bails with
# "Insufficient storage available to create either the shadow
# copy storage file." 256 MiB is the smallest size that gives
# /scan headroom and still runs in seconds.
$rawSize = 256MB
fsutil file createnew nfs.img $rawSize
./target/release/rust-ntfs.exe format -L CITEST --serial deadbeefcafe1234 nfs.img

# Dump the first 64 bytes of nfs.img BEFORE we wrap/write to the
# VHD. This is the ground-truth view of what rust-ntfs format produced.
# If this is correct but the post-remount read is wrong, the bug
# is in the VHD/PhysicalDrive layer, not mkfs_ntfs. If it's
# already wrong here, mkfs_ntfs itself produced a bad boot sector.
New-Item -ItemType Directory -Path diag -Force | Out-Null
$imgBytes = [System.IO.File]::ReadAllBytes("$pwd\nfs.img")
$hex = ($imgBytes[0..63] | ForEach-Object { '{0:X2}' -f $_ }) -join ' '
$magic = [System.Text.Encoding]::ASCII.GetString($imgBytes[3..10])
"nfs.img boot sector magic at offset 3: '$magic'" | Tee-Object diag/nfs-img-magic.txt
"first 64 bytes of nfs.img:`n$hex" | Out-File diag/nfs-img-hex.txt
# Decode the critical fields by hand so the artifact is self-explanatory.
$bps = [System.BitConverter]::ToUInt16($imgBytes, 0x0B)
$spc = $imgBytes[0x0D]
$totalSectors = [System.BitConverter]::ToUInt64($imgBytes, 0x28)
$mftLcn = [System.BitConverter]::ToUInt64($imgBytes, 0x30)
$bpbReport = "nfs.img BPB decode:`n  bytes_per_sector (0x0B): $bps`n  sectors_per_cluster (0x0D): $spc`n  total_sectors (0x28): $totalSectors`n  mft_lcn (0x30): $mftLcn"
$bpbReport | Tee-Object diag/nfs-img-bpb.txt

# Why we DON'T just convert nfs.img directly to a VHD wrapper
# without a partition table: Windows' Mount-DiskImage path
# REQUIRES a partition table on the wrapper. A raw NTFS volume
# at offset 0 (superfloppy layout) gets picked up correctly on
# physical media but disk-image mounts don't go through the
# superfloppy auto-detection.
# Confirmed by run #25230496866's Get-Disk output:
#   NumberOfPartitions: 0   PartitionStyle: MBR
# Windows saw the wrapped image as "MBR disk with zero
# partitions", not "NTFS volume". No drive letter, no chkdsk
# possible.
#
# So we build a GPT-partitioned wrapper VHD, mount it,
# initialise + add one partition aligned to 1 MiB, then
# dd our NTFS bytes into the partition's offset within
# the disk. Windows on remount sees a real partitioned
# disk with NTFS at the partition offset and mounts cleanly.
# Same layout `diskutil eraseDisk` produces — closer to the
# real shipping scenario too.

# Create an empty fixed VHD with comfortable headroom over
# the data partition. iter4 hit "specified offset is not valid"
# when the wrapper was sized too tight against the partition's
# request — Windows GPT reserves 33 LBA at each end. 384 MiB
# gives 256 MiB partition + slack with headroom to spare.
# vhd_tool create-fixed takes raw bytes (no MB suffix).
$wrapperBytes = [int64](384MB)
vhd_tool create-fixed wrapper.vhd $wrapperBytes
# Strip the NTFS host-FS sparse flag in case the host carrier
# set it (harmless on the fixed VHD, kept for parity with
# the per-scenario matrix scripts).
fsutil sparse setflag wrapper.vhd 0

# Echo file metrics for the log.
Get-Item nfs.img,wrapper.vhd | Format-List Name,Length

# Mount the empty wrapper, partition it, write our NTFS
# bytes into the partition area, dismount.
$vhd = Mount-DiskImage -ImagePath "$pwd\wrapper.vhd" -PassThru
Start-Sleep -Seconds 2
Initialize-Disk -Number $vhd.Number -PartitionStyle GPT
# Initialize-Disk completes async-ish: the LargestFreeExtent
# property doesn't refresh until we re-fetch the Disk object.
# iter5 hit "specified offset is not valid" because we used
# the stale $disk handle whose LargestFreeExtent was still 0.
Start-Sleep -Seconds 2
$disk = Get-Disk -Number $vhd.Number
Write-Host "Disk size: $($disk.Size), AllocatedSize: $($disk.AllocatedSize), LargestFreeExtent: $($disk.LargestFreeExtent)"

# Use -UseMaximumSize so we don't have to predict GPT slack
# arithmetic. Windows picks an aligned offset and the largest
# contiguous size. We'll write our nfs.img at $part.Offset
# into the disk's raw device path, which is what the prior
# write logic does anyway.
$part = New-Partition -DiskNumber $vhd.Number `
    -UseMaximumSize `
    -AssignDriveLetter:$false
Write-Host "Created partition at offset $($part.Offset), size $($part.Size)"
if ($part.Size -lt $rawSize) {
    throw "partition is smaller ($($part.Size)) than raw NTFS image ($rawSize) — bump wrapper"
}

# Open the disk's raw device path and write our NTFS bytes
# at the partition's offset. \\.\PhysicalDriveN is the
# canonical raw access path — bypasses the FS layer.
# FileAccess.ReadWrite (not just Write) so we can verify the
# write took effect by reading bytes back immediately.
$rawPath = "\\.\PhysicalDrive$($disk.Number)"
$bytes = [System.IO.File]::ReadAllBytes("$pwd\nfs.img")
$fs = [System.IO.File]::Open($rawPath,
    [System.IO.FileMode]::Open,
    [System.IO.FileAccess]::ReadWrite,
    [System.IO.FileShare]::ReadWrite)
try {
    $fs.Seek($part.Offset, [System.IO.SeekOrigin]::Begin) | Out-Null
    $fs.Write($bytes, 0, $bytes.Length)
    $fs.Flush($true)  # flush to OS + force disk write-through

    # Read first 512 bytes back at the partition offset to
    # confirm the write actually landed. NTFS boot sector
    # magic ("NTFS    " ASCII at offset 3) is unmissable.
    New-Item -ItemType Directory -Path diag -Force | Out-Null
    $fs.Seek($part.Offset, [System.IO.SeekOrigin]::Begin) | Out-Null
    $verify = New-Object byte[] 512
    $null = $fs.Read($verify, 0, 512)
    $magic = [System.Text.Encoding]::ASCII.GetString($verify, 3, 8)
    "NTFS magic at partition offset (expected 'NTFS    '): '$magic'" |
        Tee-Object diag/post-write-magic.txt
    if ($magic -ne "NTFS    ") {
        Write-Host "::warning::NTFS magic missing at partition offset — raw write may have silently failed"
        $hex = ($verify[0..63] | ForEach-Object { '{0:X2}' -f $_ }) -join ' '
        "first 64 bytes at partition offset:`n$hex" | Out-File diag/post-write-hex.txt
    }
} finally {
    $fs.Close()
}
Write-Host "Wrote $($bytes.Length) bytes of NTFS at partition offset"

# Dismount so the next step's mount goes through a fresh
# probe — Windows caches the "no partition recognized"
# state from the empty mount, and only a remount makes it
# re-scan the partition contents.
Dismount-DiskImage -ImagePath "$pwd\wrapper.vhd" | Out-Null
