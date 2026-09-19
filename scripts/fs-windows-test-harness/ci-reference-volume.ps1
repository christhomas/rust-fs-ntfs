# ci-reference-volume.ps1 -- format a second volume with Microsoft`s own
# format.com and diff its boot sector against ours, field by field.
#
# Extracted from ci.yml for the reason in ci-make-image.ps1: 107 lines of
# the Windows job`s log (#290).
$ErrorActionPreference = "Stop"

New-Item -ItemType Directory -Path diag -Force | Out-Null
$rawSize = 256MB
# Create a second VHD, partition it identically (GPT + 1 MiB
# offset), but format with Microsoft's format.exe. Dismount it
# while still mounted to capture pristine post-format bytes.
$refWrapperBytes = [int64](384MB)
vhd_tool create-fixed reference.vhd $refWrapperBytes
fsutil sparse setflag reference.vhd 0
$refVhd = Mount-DiskImage -ImagePath "$pwd\reference.vhd" -PassThru
Start-Sleep -Seconds 2
Initialize-Disk -Number $refVhd.Number -PartitionStyle GPT
Start-Sleep -Seconds 2
$refDisk = Get-Disk -Number $refVhd.Number
$refPart = New-Partition -DiskNumber $refVhd.Number `
    -UseMaximumSize -AssignDriveLetter:$true
# Wait for drive letter assignment.
Start-Sleep -Seconds 3
$refPart = Get-Partition -DiskNumber $refVhd.Number |
    Where-Object { $_.Type -ne 'Reserved' } | Select-Object -First 1
if (-not $refPart.DriveLetter) {
    "Reference partition got no drive letter — using GUID path" |
        Tee-Object diag/reference-state.txt
}
# Format with Microsoft's NTFS — quick (no zero), label CITESTREF.
# /Q skips zero-fill; /A:4096 matches our 4 KiB cluster size.
$fmtArgs = @("$($refPart.DriveLetter):", "/FS:NTFS", "/Q",
             "/A:4096", "/L", "/V:CITESTREF", "/Y")
$proc = Start-Process -FilePath "format.com" -ArgumentList $fmtArgs `
    -NoNewWindow -PassThru -Wait `
    -RedirectStandardOutput diag/reference-format.txt `
    -RedirectStandardError diag/reference-format-stderr.txt
"format.com exit: $($proc.ExitCode)" | Tee-Object diag/reference-format-exit.txt

# Dump first 64 KiB of reference partition (boot + first 16
# MFT records at 4 KiB each) AND of our partition. Both via
# the raw \\.\PhysicalDriveN path so no FS-layer interpretation.
$rawRefPath = "\\.\PhysicalDrive$($refDisk.Number)"
try {
    $fs = [System.IO.File]::Open($rawRefPath,
        [System.IO.FileMode]::Open,
        [System.IO.FileAccess]::Read,
        [System.IO.FileShare]::ReadWrite)
    # Read boot sector first to learn where MFT is.
    $fs.Seek($refPart.Offset, [System.IO.SeekOrigin]::Begin) | Out-Null
    $refBoot = New-Object byte[] 512
    $null = $fs.Read($refBoot, 0, 512)
    # Parse BPB to locate MFT.
    $refBps = [System.BitConverter]::ToUInt16($refBoot, 0x0B)
    $refSpc = $refBoot[0x0D]
    $refClusterSize = [int]$refBps * [int]$refSpc
    $refMftLcn = [System.BitConverter]::ToUInt64($refBoot, 0x30)
    $refMftFileOff = [int64]$refMftLcn * [int64]$refClusterSize
    # Dump 16 MFT records (16 * 4 KiB = 64 KiB) starting at $MFT.
    $fs.Seek($refPart.Offset + $refMftFileOff, [System.IO.SeekOrigin]::Begin) | Out-Null
    $refMft = New-Object byte[] (64KB)
    $null = $fs.Read($refMft, 0, 64KB)
    $fs.Close()
    [System.IO.File]::WriteAllBytes("$pwd\diag\reference-boot.bin", $refBoot)
    [System.IO.File]::WriteAllBytes("$pwd\diag\reference-mft-16recs.bin", $refMft)
} catch {
    "Failed to read reference bytes: $_" | Out-File diag/reference-state.txt
}
Dismount-DiskImage -ImagePath "$pwd\reference.vhd" -ErrorAction SilentlyContinue | Out-Null

# Dump same ranges from our nfs.img (already on disk).
$ourBytes = [System.IO.File]::ReadAllBytes("$pwd\nfs.img")
$ourBoot = $ourBytes[0..511]
[System.IO.File]::WriteAllBytes("$pwd\diag\ours-boot.bin", $ourBoot)
# Our MFT lives at LCN 4 = byte offset 16384.
$ourMft = $ourBytes[16384..(16384+65535)]
[System.IO.File]::WriteAllBytes("$pwd\diag\ours-mft-16recs.bin", $ourMft)

# Side-by-side hex dump of boot sectors + MFT records.
if ($refBoot -and $refBoot.Length -ge 512) {
    "Reference NTFS BPB:`n  bytes_per_sector: $refBps`n  sectors_per_cluster: $refSpc`n  cluster_size: $refClusterSize`n  mft_lcn: $refMftLcn`n  mft_file_offset: $refMftFileOff" |
        Tee-Object diag/reference-bpb.txt

    $refBootHex = ($refBoot | ForEach-Object { '{0:X2}' -f $_ }) -join ' '
    $ourBootHex = ($ourBoot | ForEach-Object { '{0:X2}' -f $_ }) -join ' '
    "REFERENCE boot sector (512 bytes):`n$refBootHex`n`nOURS boot sector (512 bytes):`n$ourBootHex" |
        Out-File diag/boot-sector-diff.txt

    # Dump each MFT record individually so the diff is per-record.
    # Records 0..F: $MFT, $MFTMirr, $LogFile, $Volume, $AttrDef,
    # root, $Bitmap, $Boot, $BadClus, $Secure, $UpCase, $Extend.
    $names = @('MFT','MFTMirr','LogFile','Volume','AttrDef',
               'root','Bitmap','Boot','BadClus','Secure',
               'UpCase','Extend','rec12','rec13','rec14','rec15')
    for ($i = 0; $i -lt 12; $i++) {
        $base = $i * 4096
        if ($refMft.Length -ge ($base + 4096) -and $ourMft.Length -ge ($base + 4096)) {
            $refRec = $refMft[$base..($base+4095)]
            $ourRec = $ourMft[$base..($base+4095)]
            $refHex = ($refRec | ForEach-Object { '{0:X2}' -f $_ }) -join ' '
            $ourHex = ($ourRec | ForEach-Object { '{0:X2}' -f $_ }) -join ' '
            $name = $names[$i]
            "REFERENCE MFT record ${i} (`$$name):`n$refHex`n`nOURS MFT record ${i} (`$$name):`n$ourHex" |
                Out-File "diag/mft-rec${i}-${name}-diff.txt"
        }
    }
}
