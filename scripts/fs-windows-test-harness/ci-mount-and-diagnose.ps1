# ci-mount-and-diagnose.ps1 -- mount the VHD, find the drive letter, and
# capture what Windows thinks of the volume before chkdsk runs.
#
# Extracted from ci.yml for the reason in ci-make-image.ps1: this body
# was 166 of the Windows job`s 1,046 log lines (#290).
#
# Sets the `letter` and `vol_path` step outputs through $env:GITHUB_OUTPUT,
# which a script inherits exactly as an inline block does.
$ErrorActionPreference = "Stop"

# Same drive-letter discovery pattern as before (snapshot,
# mount, diff). The wrapper VHD is GPT-partitioned with
# one NTFS partition now, so Windows SHOULD assign a letter
# promptly — but the snapshot-diff approach is robust to
# both the partitioned and superfloppy layouts, so keep it.
$lettersBefore = @((Get-Volume |
    Where-Object { $_.DriveLetter }).DriveLetter)

$vhd = Mount-DiskImage -ImagePath "$pwd\wrapper.vhd" -PassThru

$letter = $null
for ($i = 0; $i -lt 10; $i++) {
  Start-Sleep -Seconds 1
  $lettersAfter = @((Get-Volume |
      Where-Object { $_.DriveLetter }).DriveLetter)
  $new = $lettersAfter | Where-Object { $_ -notin $lettersBefore }
  if ($new) { $letter = $new | Select-Object -First 1; break }
}

# Always capture state — useful in success runs too for
# cross-commit baselining.
New-Item -ItemType Directory -Path diag -Force | Out-Null
Get-Disk | Format-List | Out-File diag/get-disk-on-mount.txt
Get-Volume | Format-List | Out-File diag/get-volume-on-mount.txt
Get-Partition -ErrorAction SilentlyContinue | Format-List |
    Out-File diag/get-partition-on-mount.txt

# Re-read the first 512 bytes at the partition offset post-remount
# to verify the bytes actually persisted across dismount/remount.
# If this magic differs from pre-dismount, VHD or Windows ate them.
$disk = Get-Disk -Number $vhd.Number
$partition = Get-Partition -DiskNumber $disk.Number |
    Where-Object { $_.Type -ne 'Reserved' } | Select-Object -First 1
if ($partition) {
    $rawPath = "\\.\PhysicalDrive$($disk.Number)"
    try {
        $fs = [System.IO.File]::Open($rawPath,
            [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::Read,
            [System.IO.FileShare]::ReadWrite)
        $fs.Seek($partition.Offset, [System.IO.SeekOrigin]::Begin) | Out-Null
        $verify = New-Object byte[] 512
        $null = $fs.Read($verify, 0, 512)
        $fs.Close()
        $magic = [System.Text.Encoding]::ASCII.GetString($verify, 3, 8)
        "Post-remount NTFS magic at partition offset $($partition.Offset): '$magic'" |
            Tee-Object diag/post-remount-magic.txt
        $hex = ($verify[0..63] | ForEach-Object { '{0:X2}' -f $_ }) -join ' '
        "first 64 bytes of partition post-remount:`n$hex" | Out-File diag/post-remount-hex.txt
    } catch {
        "Failed to read raw partition bytes post-remount: $_" |
            Out-File diag/post-remount-magic.txt
    }
}

if (-not $letter) {
  # No auto-assignment — try forcing one before giving up.
  # Windows will sometimes leave a recognised volume unmapped
  # when the disk is non-removable VHD-backed.
  $partition = Get-Partition -DiskNumber $disk.Number |
      Where-Object { $_.Type -ne 'Reserved' } | Select-Object -First 1
  if ($partition) {
      # Find an unused letter D..Z.
      $used = (Get-Volume | ForEach-Object { $_.DriveLetter }) +
              (Get-PSDrive -PSProvider FileSystem | ForEach-Object { $_.Name })
      foreach ($c in [char[]](68..90)) {
          if ($c -notin $used) {
              try {
                  Set-Partition -DiskNumber $disk.Number `
                      -PartitionNumber $partition.PartitionNumber `
                      -NewDriveLetter $c -ErrorAction Stop
                  $letter = "$c"
                  Write-Host "Forced drive letter $letter via Set-Partition"
                  break
              } catch {
                  "Set-Partition with letter $c failed: $_" |
                      Out-File diag/set-partition-attempts.txt -Append
              }
          }
      }
  }
}

if (-not $letter) {
  # Still no letter — capture state and fall back to volume-guid path.
  Get-Disk | Format-List | Out-File diag/get-disk-on-failure.txt
  Get-Volume | Format-List | Out-File diag/get-volume-on-failure.txt
  Get-Partition -ErrorAction SilentlyContinue | Format-List |
      Out-File diag/get-partition-on-failure.txt
  throw "wrapper VHD mounted but no drive letter was assigned and Set-Partition fallback failed — Windows did not recognise the NTFS partition we wrote. See diag/ for storage state dumps."
}

Write-Host "Mounted at ${letter}:"
# Stash the drive letter for later steps.
"letter=$letter" | Out-File -FilePath $env:GITHUB_OUTPUT -Append

# Stash the volume guid path too, so chkdsk can fall back to
# \\?\Volume{guid}\ if the drive-letter path can't be parsed.
$volPath = "\\?\Volume$($partition.Guid)\"
"vol_path=$volPath" | Out-File -FilePath $env:GITHUB_OUTPUT -Append

# Capture as much "what does Windows think this volume is?"
# metadata as we can BEFORE chkdsk runs. None of these are
# allowed to fail the step — the chkdsk step is the gate.
New-Item -ItemType Directory -Path diag -Force | Out-Null
Get-Volume -DriveLetter $letter -ErrorAction SilentlyContinue |
    Format-List * | Out-File diag/get-volume.txt
# 2>&1 + |out captures stderr from external tools (fsutil writes
# 'Error 1393' to stderr on disk-corrupt) into the file.
(& fsutil fsinfo volumeinfo "${letter}:" 2>&1) | Out-File diag/fsutil-volumeinfo.txt
(& fsutil fsinfo statistics "${letter}:" 2>&1) | Out-File diag/fsutil-statistics.txt
(Get-ChildItem "${letter}:\" -Force -ErrorAction SilentlyContinue) |
    Out-File diag/root-listing.txt

# Dump the boot signature (0x55AA at 0x1FE-0x1FF) and full
# 512-byte boot sector — narrows down whether mkfs_ntfs missed
# a Windows-specific field beyond what we already inspected.
$img = [System.IO.File]::ReadAllBytes("$pwd\nfs.img")
$sig = '{0:X2} {1:X2}' -f $img[0x1FE], $img[0x1FF]
"boot signature at 0x1FE-0x1FF (expected '55 AA'): $sig" |
    Tee-Object diag/boot-signature.txt
$allHex = ($img[0..511] | ForEach-Object { '{0:X2}' -f $_ }) -join ' '
"full boot sector (512 bytes):`n$allHex" | Out-File diag/boot-sector-full.txt
# Compare backup-boot location: NTFS backup at last sector of
# volume (volume_size - 512). Read the backup sector via the
# raw partition path on the wrapper.
$rawPath = "\\.\PhysicalDrive$($disk.Number)"
try {
    $fs = [System.IO.File]::Open($rawPath,
        [System.IO.FileMode]::Open,
        [System.IO.FileAccess]::Read,
        [System.IO.FileShare]::ReadWrite)
    # Last sector of the partition.
    $backupOff = $partition.Offset + $partition.Size - 512
    $fs.Seek($backupOff, [System.IO.SeekOrigin]::Begin) | Out-Null
    $backup = New-Object byte[] 512
    $null = $fs.Read($backup, 0, 512)
    $fs.Close()
    $bMagic = [System.Text.Encoding]::ASCII.GetString($backup, 3, 8)
    "Backup boot magic at last sector (offset $backupOff): '$bMagic'" |
        Tee-Object diag/backup-boot-magic.txt
    $bHex = ($backup[0..63] | ForEach-Object { '{0:X2}' -f $_ }) -join ' '
    "first 64 bytes of last sector:`n$bHex" | Out-File diag/backup-boot-hex.txt
} catch {
    "Failed to read backup boot: $_" | Out-File diag/backup-boot-magic.txt
}

# Read the Windows Event Log for filesystem events while this
# volume was being probed. Captures the kernel's reason for
# rejecting the volume (e.g. NTFS event 55 'corrupt MFT').
try {
    Get-WinEvent -LogName 'System' -MaxEvents 100 -ErrorAction SilentlyContinue |
        Where-Object {
            $_.ProviderName -in 'Ntfs','Microsoft-Windows-Ntfs','Disk','Volsnap','partmgr'
        } |
        Select-Object TimeCreated, ProviderName, Id, LevelDisplayName, Message |
        Format-List | Out-File diag/eventlog-fs.txt
} catch {
    "Failed to query event log: $_" | Out-File diag/eventlog-fs.txt
}
