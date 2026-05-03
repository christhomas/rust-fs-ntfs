# run-write-smoke.ps1 -- minimal mkfs → mount → write-one-file diagnostic.
#
# Skips chkdsk entirely. Answers: when ntfs.sys actually tries to write
# to a freshly mkfs'd volume, what specifically fails?
#
# Output contract: WRITE_OK=<0|1> on stdout. Everything else lands in $Diag:
#   $Diag/
#     drive-letter.txt
#     mount-eventlog.txt        # NTFS/Disk events from the mount attempt
#     write-attempt.txt         # what we tried + the .NET exception (if any)
#     write-eventlog.txt        # NTFS events fired during/after the write
#     write-exit.txt            # 0=succeeded, 1=failed (matches WRITE_OK)
#     get-volume.txt            # ntfs.sys's view (FileSystemType / Size etc)
#     fsutil-fsinfo.txt         # fsutil fsinfo ntfsinfo dump
#     post-write-boot.bin       # boot sector AFTER the write attempt
#     post-write-mft-16recs.bin # first 16 MFT records AFTER the write attempt

param(
    [Parameter(Mandatory=$true)] [string]$Img,
    [Parameter(Mandatory=$true)] [string]$Vhdx,
    [Parameter(Mandatory=$true)] [string]$Diag,
    [Parameter(Mandatory=$true)] [int]$VolumeSizeMb
)

$ErrorActionPreference = 'Continue'
$env:PATH = "C:\Program Files\Cloudbase Solutions\QEMU\bin;$env:PATH"

$rawSize = $VolumeSizeMb * 1MB
$wrapperSizeMb = $VolumeSizeMb + 128

New-Item -ItemType Directory -Path $Diag -Force | Out-Null

foreach ($v in @($Vhdx)) {
    try {
        Get-DiskImage -ImagePath $v -EA SilentlyContinue |
            Where-Object Attached |
            Dismount-DiskImage -EA SilentlyContinue | Out-Null
    } catch { }
}
Remove-Item -LiteralPath $Vhdx -Force -EA SilentlyContinue

# Marker so we can filter the System log for events that fired DURING this run.
$startTime = Get-Date

try {
    # ── Wrap nfs.img into a GPT VHDX ──────────────────────────────────
    & qemu-img create -f vhdx -o subformat=fixed $Vhdx "${wrapperSizeMb}M" | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "qemu-img create failed exit=$LASTEXITCODE" }
    fsutil sparse setflag $Vhdx 0 | Out-Null

    $vhd = Mount-DiskImage -ImagePath $Vhdx -PassThru
    Start-Sleep -Seconds 2
    Initialize-Disk -Number $vhd.Number -PartitionStyle GPT
    Start-Sleep -Seconds 2
    $disk = Get-Disk -Number $vhd.Number
    $part = New-Partition -DiskNumber $vhd.Number -UseMaximumSize -AssignDriveLetter:$false
    if ($part.Size -lt $rawSize) { throw "partition smaller than raw image" }

    $rawPath = "\\.\PhysicalDrive$($disk.Number)"
    $ourBytes = [System.IO.File]::ReadAllBytes($Img)
    $fs = [System.IO.File]::Open($rawPath, [System.IO.FileMode]::Open,
        [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::ReadWrite)
    try {
        $fs.Seek($part.Offset, [System.IO.SeekOrigin]::Begin) | Out-Null
        $fs.Write($ourBytes, 0, $ourBytes.Length)
        $fs.Flush($true)
    } finally { $fs.Close() }
    Dismount-DiskImage -ImagePath $Vhdx | Out-Null

    # ── Re-mount; let ntfs.sys recognise the populated partition ──────
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
    if (-not $letter) { throw 'no drive letter assigned' }
    "$letter" | Out-File "$Diag\drive-letter.txt" -Encoding ASCII

    # ── Capture mount-time view of the volume ─────────────────────────
    try {
        Get-Volume -DriveLetter $letter | Format-List | Out-File "$Diag\get-volume.txt"
    } catch {
        "Get-Volume failed: $_" | Out-File "$Diag\get-volume.txt"
    }
    try {
        & fsutil fsinfo ntfsinfo "${letter}:" *> "$Diag\fsutil-fsinfo.txt"
    } catch { }

    # NTFS events that fired during the mount itself (Event 55 etc).
    try {
        Get-WinEvent -LogName 'System' -EA SilentlyContinue |
            Where-Object {
                $_.TimeCreated -ge $startTime -and
                $_.ProviderName -in 'Ntfs','Microsoft-Windows-Ntfs','Disk','Volsnap','partmgr'
            } |
            Select-Object TimeCreated, ProviderName, Id, LevelDisplayName, Message |
            Format-List | Out-File "$Diag\mount-eventlog.txt"
    } catch {
        "mount-eventlog capture failed: $_" | Out-File "$Diag\mount-eventlog.txt"
    }

    # ── The actual write attempt ──────────────────────────────────────
    $writeStart = Get-Date
    $writeOk = 0
    $writeReport = New-Object System.Text.StringBuilder
    $writeTarget = "${letter}:\smoke.txt"
    [void]$writeReport.AppendLine("attempt: [System.IO.File]::WriteAllText('$writeTarget', 'hello smoke')")
    [void]$writeReport.AppendLine("started: $writeStart")
    try {
        [System.IO.File]::WriteAllText($writeTarget, "hello smoke`r`n")
        # Confirm the file is actually visible after the write returns.
        if (Test-Path -LiteralPath $writeTarget) {
            $len = (Get-Item -LiteralPath $writeTarget).Length
            [void]$writeReport.AppendLine("result : SUCCESS (file present, $len bytes)")
            $writeOk = 1
        } else {
            [void]$writeReport.AppendLine("result : NO-EXCEPTION but file not present after write")
        }
    } catch {
        $ex = $_.Exception
        [void]$writeReport.AppendLine("result : EXCEPTION")
        [void]$writeReport.AppendLine("type   : $($ex.GetType().FullName)")
        [void]$writeReport.AppendLine("message: $($ex.Message)")
        if ($ex.InnerException) {
            [void]$writeReport.AppendLine("inner-type   : $($ex.InnerException.GetType().FullName)")
            [void]$writeReport.AppendLine("inner-message: $($ex.InnerException.Message)")
        }
        if ($ex.HResult) {
            [void]$writeReport.AppendLine(("hresult: 0x{0:X8} ({0})" -f $ex.HResult))
        }
        [void]$writeReport.AppendLine("--- stack ---")
        [void]$writeReport.AppendLine($_.ScriptStackTrace)
    }
    $writeReport.ToString() | Out-File "$Diag\write-attempt.txt"
    "$writeOk" | Out-File "$Diag\write-exit.txt" -Encoding ASCII

    # NTFS events fired during/after the write (Event 137 = transactional
    # log corruption, Event 55 = generic corruption, Event 50 = delayed
    # write failure, etc — all diagnostic).
    try {
        Get-WinEvent -LogName 'System' -EA SilentlyContinue |
            Where-Object {
                $_.TimeCreated -ge $writeStart -and
                $_.ProviderName -in 'Ntfs','Microsoft-Windows-Ntfs','Disk','Volsnap','partmgr'
            } |
            Select-Object TimeCreated, ProviderName, Id, LevelDisplayName, Message |
            Format-List | Out-File "$Diag\write-eventlog.txt"
    } catch {
        "write-eventlog capture failed: $_" | Out-File "$Diag\write-eventlog.txt"
    }

    # ── Dump post-write boot + first 16 MFT records ───────────────────
    # We dismount the volume cleanly first so any pending writes are
    # flushed back into the underlying VHDX, then re-open the raw
    # PhysicalDrive and read the bytes back. This shows exactly what
    # ntfs.sys actually persisted (or didn't).
    try { Dismount-DiskImage -ImagePath $Vhdx | Out-Null } catch { }
    Start-Sleep -Seconds 1
    try {
        $reMounted = Mount-DiskImage -ImagePath $Vhdx -PassThru
        Start-Sleep -Seconds 2
        $reDisk = Get-Disk -Number $reMounted.Number
        $rePart = Get-Partition -DiskNumber $reDisk.Number |
            Where-Object { $_.Type -ne 'Reserved' } | Select-Object -First 1
        $reRaw = "\\.\PhysicalDrive$($reDisk.Number)"
        $reFs = [System.IO.File]::Open($reRaw, [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
        try {
            $reFs.Seek($rePart.Offset, [System.IO.SeekOrigin]::Begin) | Out-Null
            $bootBuf = New-Object byte[] 512
            $null = $reFs.Read($bootBuf, 0, 512)
            $bps = [System.BitConverter]::ToUInt16($bootBuf, 0x0B)
            $spc = $bootBuf[0x0D]
            $mftLcn = [System.BitConverter]::ToUInt64($bootBuf, 0x30)
            $clusterSize = [int]$bps * [int]$spc
            $mftFileOff = [int64]$mftLcn * [int64]$clusterSize
            [System.IO.File]::WriteAllBytes("$Diag\post-write-boot.bin", $bootBuf)
            $reFs.Seek($rePart.Offset + $mftFileOff, [System.IO.SeekOrigin]::Begin) | Out-Null
            $mftBuf = New-Object byte[] (64KB)
            $null = $reFs.Read($mftBuf, 0, 64KB)
            [System.IO.File]::WriteAllBytes("$Diag\post-write-mft-16recs.bin", $mftBuf)
        } finally { $reFs.Close() }
    } catch {
        "post-write dump failed: $_" | Out-File "$Diag\post-write-dump-error.txt"
    }

    Write-Output "WRITE_OK=$writeOk"
} finally {
    foreach ($v in @($Vhdx)) {
        try {
            Get-DiskImage -ImagePath $v -EA SilentlyContinue |
                Where-Object Attached |
                Dismount-DiskImage -EA SilentlyContinue | Out-Null
        } catch { }
    }
}
