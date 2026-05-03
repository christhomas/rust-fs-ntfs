# run-scenario.ps1 -- Windows-side scenario executor.
#
# Invoked by tests/matrix.rs (one process per scenario). Does the full
# wrap → mount → reference-format → chkdsk → event-log lifecycle and
# writes a complete evidence packet under $Diag.
#
# Output contract: writes RO_EXIT=<n> / SCAN_EXIT=<n> lines to stdout.
# The Rust harness greps these to recover chkdsk's verdict; everything
# else lands as files under $Diag for an agent to inspect.
#
# Diag layout produced (consumed by results-aggregator + downstream agents):
#   $Diag/
#     mkfs-stdout.txt           # already written by Rust before invocation
#     mkfs-stderr.txt           # already written by Rust before invocation
#     qemu-create.txt           # qemu-img stdout for our wrapper
#     qemu-create-reference.txt # qemu-img stdout for reference VHDX
#     drive-letter.txt          # mounted drive letter (e.g. "F")
#     ours-boot.bin             # 512 bytes — first sector of our nfs.img
#     ours-mft-16recs.bin       # 16 × 4 KiB MFT records starting at our $MFT
#     ours-bpb.txt              # parsed BPB summary
#     reference-boot.bin        # same, from format.com reference
#     reference-mft-16recs.bin
#     reference-bpb.txt
#     reference-format.txt      # format.com stdout
#     chkdsk-readonly.txt       # chkdsk DRIVE: output
#     chkdsk-scan.txt           # chkdsk DRIVE: /scan output
#     chkdsk-readonly-exit.txt  # exit code marker
#     chkdsk-scan-exit.txt
#     eventlog-ntfs.txt         # NTFS / Disk / partmgr System log entries
#     ps-stdout.txt             # this script's stdout (written by Rust)
#     ps-stderr.txt             # this script's stderr (written by Rust)

param(
    [Parameter(Mandatory=$true)] [string]$Img,
    [Parameter(Mandatory=$true)] [string]$Vhdx,
    [Parameter(Mandatory=$true)] [string]$ReferenceVhdx,
    [Parameter(Mandatory=$true)] [string]$Diag,
    [Parameter(Mandatory=$true)] [AllowEmptyString()] [string]$Label,
    [Parameter(Mandatory=$true)] [int]$ClusterSize,
    [Parameter(Mandatory=$true)] [int]$VolumeSizeMb,
    # Tier-3: after the initial mount + chkdsk, dismount/remount this
    # many extra times, running chkdsk between each cycle. Catches
    # bugs where the volume only goes inconsistent after multiple
    # clean dismounts. Default 0 = no extra cycles, behaves as before.
    [int]$RemountCycles = 0,
    # Optional path to a JSON file describing fixture files to create
    # on the mounted volume after mount, before chkdsk. Schema:
    #   [
    #     {"name": "tiny.txt", "content": "hello world"},
    #     {"name": "big.bin", "size_bytes": 4096, "content_pattern": "zeros"}
    #   ]
    # Empty array / missing file = no win-side writes (the default for
    # legacy scenarios).
    [string]$FixturesJson = ""
)

$ErrorActionPreference = 'Continue'
$env:PATH = "C:\Program Files\Cloudbase Solutions\QEMU\bin;$env:PATH"

$rawSize = $VolumeSizeMb * 1MB
$wrapperSizeMb = $VolumeSizeMb + 128

New-Item -ItemType Directory -Path $Diag -Force | Out-Null

# Best-effort cleanup of any leftover mounts on these specific images.
foreach ($v in @($Vhdx, $ReferenceVhdx)) {
    try {
        Get-DiskImage -ImagePath $v -EA SilentlyContinue |
            Where-Object Attached |
            Dismount-DiskImage -EA SilentlyContinue | Out-Null
    } catch { }
}
Remove-Item -LiteralPath $Vhdx, $ReferenceVhdx -Force -EA SilentlyContinue

# Also wipe stale VHDX/.img files from prior scenarios in this run
# — they accumulate on the VM's C: drive and the largest scenarios
# (1 GiB volume + 128 MiB wrapper) fail with "SetEndOfFile error: 112"
# (ERROR_DISK_FULL) once the cumulative total fills available space.
# Don't touch the current scenario's files (we're about to write
# them) and don't touch the matrix workspace itself.
$matrixWorkdir = Split-Path $Vhdx -Parent
if ($matrixWorkdir) {
    $keepNames = @((Split-Path $Vhdx -Leaf), (Split-Path $ReferenceVhdx -Leaf), (Split-Path $Img -Leaf))
    Get-ChildItem -Path $matrixWorkdir -Filter '*.vhdx' -EA SilentlyContinue |
        Where-Object { $_.Name -notin $keepNames } |
        ForEach-Object {
            try {
                Get-DiskImage -ImagePath $_.FullName -EA SilentlyContinue |
                    Where-Object Attached |
                    Dismount-DiskImage -EA SilentlyContinue | Out-Null
            } catch { }
            Remove-Item -LiteralPath $_.FullName -Force -EA SilentlyContinue
        }
    Get-ChildItem -Path $matrixWorkdir -Filter 'nfs-*.img' -EA SilentlyContinue |
        Where-Object { $_.Name -notin $keepNames } |
        Remove-Item -Force -EA SilentlyContinue
}

try {
    # ── Stage A: wrap our nfs.img into a GPT-partitioned VHDX ────────
    & qemu-img create -f vhdx -o subformat=fixed $Vhdx "${wrapperSizeMb}M" |
        Out-File "$Diag\qemu-create.txt"
    if ($LASTEXITCODE -ne 0) { throw "qemu-img create (wrapper) failed exit=$LASTEXITCODE" }
    fsutil sparse setflag $Vhdx 0 | Out-Null

    $vhd = Mount-DiskImage -ImagePath $Vhdx -PassThru
    Start-Sleep -Seconds 2
    Initialize-Disk -Number $vhd.Number -PartitionStyle GPT
    Start-Sleep -Seconds 2
    $disk = Get-Disk -Number $vhd.Number
    $part = New-Partition -DiskNumber $vhd.Number -UseMaximumSize -AssignDriveLetter:$false
    if ($part.Size -lt $rawSize) { throw "partition smaller than raw image" }

    $rawPath = "\\.\PhysicalDrive$($disk.Number)"
    # Stream-copy the image into the partition (chunked) — `ReadAllBytes`
    # is capped at 2 GiB by PowerShell's CLR, so 4 GiB / 16 GiB scenarios
    # fail there. Use FileStream + a 16 MiB buffer instead.
    $imgFs = [System.IO.File]::Open($Img, [System.IO.FileMode]::Open,
        [System.IO.FileAccess]::Read, [System.IO.FileShare]::Read)
    $fs = [System.IO.File]::Open($rawPath, [System.IO.FileMode]::Open,
        [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::ReadWrite)
    try {
        $fs.Seek($part.Offset, [System.IO.SeekOrigin]::Begin) | Out-Null
        $bufSize = 16 * 1024 * 1024
        $buf = New-Object byte[] $bufSize
        while ($true) {
            $n = $imgFs.Read($buf, 0, $bufSize)
            if ($n -le 0) { break }
            $fs.Write($buf, 0, $n)
        }
        $fs.Flush($true)
    } finally {
        $fs.Close()
        $imgFs.Close()
    }
    Dismount-DiskImage -ImagePath $Vhdx | Out-Null

    # ── Stage B: re-mount; Windows recognises the populated partition ─
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

    # Capture this scenario's volume GUID + start-of-mount timestamp so
    # the eventlog dump in Stage F can filter to ONLY this scenario's
    # events. Without this, the matrix's 200-event window sweeps in
    # events from prior scenarios in the same run, mixing them together
    # and making per-scenario diagnosis ambiguous.
    $scenarioStart = Get-Date
    $volumeGuid = $null
    try {
        $vol = Get-Volume -DriveLetter $letter -EA SilentlyContinue
        if ($vol) {
            $volumeGuid = $vol.UniqueId
            "$volumeGuid" | Out-File "$Diag\volume-guid.txt" -Encoding ASCII
        }
    } catch { }

    # ── Stage B2: apply win-side fixtures (Tier-2 win:write support) ───
    # Fixtures populate files on the mounted volume before the chkdsk
    # pass, so chkdsk validates the post-write state. Schema declared in
    # the param-block header. Empty array / missing file is the no-op
    # path (legacy mac:format -> win:chkdsk* scenarios).
    if ($FixturesJson -and (Test-Path $FixturesJson)) {
        try {
            $fixtures = Get-Content -Raw $FixturesJson | ConvertFrom-Json
            $applied = New-Object System.Collections.Generic.List[string]
            foreach ($fx in @($fixtures)) {
                $dest = "${letter}:\$($fx.name)"
                if ($null -ne $fx.content) {
                    # Inline UTF-8 content. -NoNewline so the bytes match
                    # the declared content exactly (no trailing CRLF).
                    Set-Content -LiteralPath $dest -Value $fx.content -NoNewline -Encoding UTF8
                    $applied.Add("text $($fx.name) ($([System.Text.Encoding]::UTF8.GetByteCount($fx.content)) B)")
                } elseif ($null -ne $fx.size_bytes) {
                    $n = [int]$fx.size_bytes
                    $pat = if ($fx.content_pattern) { "$($fx.content_pattern)" } else { "zeros" }
                    $buf = New-Object byte[] $n
                    switch ($pat) {
                        "zeros"        { } # already zeroed
                        "ones"         { for ($i = 0; $i -lt $n; $i++) { $buf[$i] = 0xFF } }
                        "incrementing" { for ($i = 0; $i -lt $n; $i++) { $buf[$i] = [byte]($i -band 0xFF) } }
                        "random"       {
                            $rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
                            $rng.GetBytes($buf)
                            $rng.Dispose()
                        }
                        default        { throw "unknown content_pattern: $pat" }
                    }
                    [System.IO.File]::WriteAllBytes($dest, $buf)
                    $applied.Add("bytes $($fx.name) ($n B, $pat)")
                } else {
                    throw "fixture $($fx.name) has neither 'content' nor 'size_bytes'"
                }
            }
            $applied -join "`n" | Out-File "$Diag\fixtures-applied.txt" -Encoding UTF8
        } catch {
            "fixture application failed: $_" | Out-File "$Diag\fixtures-error.txt" -Encoding UTF8
            throw
        }
    }

    # ── Stage C: dump our boot sector + MFT records (byte-diff input) ──
    $bps = [System.BitConverter]::ToUInt16($ourBytes, 0x0B)
    $spc = $ourBytes[0x0D]
    $mftLcn = [System.BitConverter]::ToUInt64($ourBytes, 0x30)
    $ourClusterSize = [int]$bps * [int]$spc
    $ourMftOff = [int64]$mftLcn * [int64]$ourClusterSize
    $ourMftEnd = [int64]$ourMftOff + 65535
    [System.IO.File]::WriteAllBytes("$Diag\ours-boot.bin", $ourBytes[0..511])
    [System.IO.File]::WriteAllBytes("$Diag\ours-mft-16recs.bin", $ourBytes[$ourMftOff..$ourMftEnd])
    "ours BPB: bps=$bps spc=$spc cluster=$ourClusterSize mft_lcn=$mftLcn mft_off=$ourMftOff" |
        Out-File "$Diag\ours-bpb.txt"

    # Dump our $LogFile content. mkfs places $LogFile at LCN
    # mft_lcn + mft_clusters where mft_clusters = ceil(64 records *
    # mft_record_size / cluster_size). With our defaults (4096
    # cluster, 4096 mft_record_size), mft_clusters = 64; so $LogFile
    # starts at LCN mft_lcn + 64. Size is 64 KiB (16 clusters at 4096).
    $ourMftRecBytes = [System.BitConverter]::ToInt32($ourBytes, 0x40)
    if ($ourMftRecBytes -lt 0) {
        $ourMftRecBytes = 1 -shl ([byte](-$ourMftRecBytes))
    } else {
        $ourMftRecBytes = $ourMftRecBytes * $ourClusterSize
    }
    $ourMftClusters = [int][Math]::Ceiling((64 * $ourMftRecBytes) / [double]$ourClusterSize)
    $ourLogfileLcn = $mftLcn + $ourMftClusters
    $ourLogfileOff = [int64]$ourLogfileLcn * [int64]$ourClusterSize
    $ourLogfileLen = 65536  # mkfs fixes $LogFile at 64 KiB
    $ourLogfileEnd = [int64]$ourLogfileOff + $ourLogfileLen - 1
    if ($ourLogfileEnd -lt $ourBytes.Length) {
        [System.IO.File]::WriteAllBytes("$Diag\ours-logfile.bin",
            $ourBytes[$ourLogfileOff..$ourLogfileEnd])
        "ours LogFile: lcn=$ourLogfileLcn off=$ourLogfileOff len=$ourLogfileLen" |
            Out-File "$Diag\ours-logfile-info.txt"
    }

    # Walk our MFT rec 4 ($AttrDef) to dump its $DATA cluster too.
    $ourAttrRec = $ourBytes[($ourMftOff + 4 * $ourMftRecBytes)..($ourMftOff + 5 * $ourMftRecBytes - 1)]
    $atOff = [System.BitConverter]::ToUInt16($ourAttrRec, 0x14)
    $cur = $atOff
    $ourAttrdefLcn = $null
    $ourAttrdefLen = 4096
    while ($cur -lt $ourAttrRec.Length - 8) {
        $atype = [System.BitConverter]::ToUInt32($ourAttrRec, $cur)
        if ($atype -eq 0xFFFFFFFF) { break }
        $alen = [System.BitConverter]::ToUInt32($ourAttrRec, $cur + 4)
        if ($alen -eq 0) { break }
        $nonRes = $ourAttrRec[$cur + 8]
        if ($atype -eq 0x80 -and $nonRes -eq 1) {
            $mpOff = [System.BitConverter]::ToUInt16($ourAttrRec, $cur + 0x20)
            $mp = $cur + $mpOff
            $hdr = $ourAttrRec[$mp]
            $lenSize = $hdr -band 0x0F
            $lcnSize = ($hdr -shr 4) -band 0x0F
            if ($lenSize -gt 0 -and $lcnSize -gt 0) {
                $lcnRaw = $ourAttrRec[($mp + 1 + $lenSize)..($mp + $lenSize + $lcnSize)]
                [int64]$lcnDelta = 0
                for ($k = $lcnSize - 1; $k -ge 0; $k--) {
                    $lcnDelta = ($lcnDelta -shl 8) -bor [int64]$lcnRaw[$k]
                }
                $signBit = 1 -shl ($lcnSize * 8 - 1)
                if ($lcnDelta -band $signBit) {
                    $lcnDelta = $lcnDelta -bor (-1 -shl ($lcnSize * 8))
                }
                $ourAttrdefLcn = $lcnDelta
            }
            $ourAttrdefLen = [System.BitConverter]::ToInt64($ourAttrRec, $cur + 0x30)
            if ($ourAttrdefLen -gt 65536) { $ourAttrdefLen = 65536 }
            break
        }
        $cur += $alen
    }
    if ($ourAttrdefLcn -ne $null) {
        $ourAttrdefOff = [int64]$ourAttrdefLcn * [int64]$ourClusterSize
        $ourAttrdefEnd = $ourAttrdefOff + $ourAttrdefLen - 1
        if ($ourAttrdefEnd -lt $ourBytes.Length) {
            [System.IO.File]::WriteAllBytes("$Diag\ours-attrdef.bin",
                $ourBytes[$ourAttrdefOff..$ourAttrdefEnd])
            "ours AttrDef: lcn=$ourAttrdefLcn off=$ourAttrdefOff len=$ourAttrdefLen" |
                Out-File "$Diag\ours-attrdef-info.txt"
        }
    }

    # ── Stage D: format.com reference at matching params (byte-diff target) ─
    & qemu-img create -f vhdx -o subformat=fixed $ReferenceVhdx "${wrapperSizeMb}M" |
        Out-File "$Diag\qemu-create-reference.txt"
    if ($LASTEXITCODE -ne 0) { throw "qemu-img create (reference) failed exit=$LASTEXITCODE" }
    fsutil sparse setflag $ReferenceVhdx 0 | Out-Null

    $refVhd = Mount-DiskImage -ImagePath $ReferenceVhdx -PassThru
    Start-Sleep -Seconds 2
    Initialize-Disk -Number $refVhd.Number -PartitionStyle GPT
    Start-Sleep -Seconds 2
    $refDisk = Get-Disk -Number $refVhd.Number
    $null = New-Partition -DiskNumber $refVhd.Number -UseMaximumSize -AssignDriveLetter:$true
    Start-Sleep -Seconds 3
    $refPart = Get-Partition -DiskNumber $refVhd.Number |
        Where-Object { $_.Type -ne 'Reserved' } | Select-Object -First 1
    # /A:$ClusterSize matches our scenario's cluster size — without
    # this, byte-diff at non-default cluster sizes is meaningless.
    # Reference label fixed (CITESTREF) — we're comparing layout, not
    # label encoding.
    # format.com rejects `/A:65536` numerically — use K-suffix syntax
    # for cluster sizes >= 16 KiB. (Verified: matrix run-20260503-030521
    # cluster-64k captured "Invalid parameter - /A:65536" from format.com.)
    $clusterArg = if ($ClusterSize -ge 16384 -and $ClusterSize % 1024 -eq 0) {
        "/A:$($ClusterSize / 1024)K"
    } else {
        "/A:$ClusterSize"
    }
    $fmtArgs = @("$($refPart.DriveLetter):", "/FS:NTFS", "/Q",
                 $clusterArg, "/L", "/V:CITESTREF", "/Y")
    $fp = Start-Process -FilePath "format.com" -ArgumentList $fmtArgs `
        -NoNewWindow -PassThru -Wait `
        -RedirectStandardOutput "$Diag\reference-format.txt"
    "format.com exit: $($fp.ExitCode)" | Out-File "$Diag\reference-format-exit.txt"

    $refRaw = "\\.\PhysicalDrive$($refDisk.Number)"
    $refsh = [System.IO.File]::Open($refRaw, [System.IO.FileMode]::Open,
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
        [System.IO.File]::WriteAllBytes("$Diag\reference-boot.bin", $refBoot)
        [System.IO.File]::WriteAllBytes("$Diag\reference-mft-16recs.bin", $refMft)

        # Dump first 64 MFT records (256 KiB) so we can see $Extend's
        # children — $RmMetadata, $Repair, $TxfLog, $TxfLog\$TxfLog.blf,
        # $Txf, $Tops, etc. — that format.com places past slot 15.
        $refsh.Seek($refPart.Offset + $refMftFileOff, [System.IO.SeekOrigin]::Begin) | Out-Null
        $refMft64 = New-Object byte[] (256KB)
        $null = $refsh.Read($refMft64, 0, 256KB)
        [System.IO.File]::WriteAllBytes("$Diag\reference-mft-64recs.bin", $refMft64)
        "ref BPB: bps=$refBps spc=$refSpc cluster=$refClusterSize mft_lcn=$refMftLcn mft_off=$refMftFileOff" |
            Out-File "$Diag\reference-bpb.txt"

        # Walk reference's $LogFile MFT record (slot 2) for its $DATA
        # data run; resolve to LCN, read first 64 KiB. Reference may
        # place $LogFile anywhere — we cannot assume mft_lcn+N like
        # ours. Parse MFT rec 2 to get the $DATA non-resident mapping.
        $refLogRec = $refMft[(2 * 4096)..(3 * 4096 - 1)]
        # MFT record header: attrs_offset is at 0x14 (u16 LE).
        $attrsOff = [System.BitConverter]::ToUInt16($refLogRec, 0x14)
        $cur = $attrsOff
        $logfileLcn = $null
        $logfileLen = 65536
        while ($cur -lt $refLogRec.Length - 8) {
            $atype = [System.BitConverter]::ToUInt32($refLogRec, $cur)
            if ($atype -eq 0xFFFFFFFF) { break }
            $alen = [System.BitConverter]::ToUInt32($refLogRec, $cur + 4)
            if ($alen -eq 0) { break }
            $nonRes = $refLogRec[$cur + 8]
            if ($atype -eq 0x80 -and $nonRes -eq 1) {
                # Non-resident $DATA. Mapping pairs offset is at +0x20.
                $mpOff = [System.BitConverter]::ToUInt16($refLogRec, $cur + 0x20)
                $mp = $cur + $mpOff
                # First mapping pair: header byte = (lcn_size<<4) | len_size
                $hdr = $refLogRec[$mp]
                $lenSize = $hdr -band 0x0F
                $lcnSize = ($hdr -shr 4) -band 0x0F
                if ($lenSize -gt 0 -and $lcnSize -gt 0) {
                    $lenBytes = $refLogRec[($mp + 1)..($mp + $lenSize)]
                    # Sign-extend the LCN delta (signed)
                    $lcnRaw = $refLogRec[($mp + 1 + $lenSize)..($mp + $lenSize + $lcnSize)]
                    [int64]$lcnDelta = 0
                    for ($k = $lcnSize - 1; $k -ge 0; $k--) {
                        $lcnDelta = ($lcnDelta -shl 8) -bor [int64]$lcnRaw[$k]
                    }
                    $signBit = 1 -shl ($lcnSize * 8 - 1)
                    if ($lcnDelta -band $signBit) {
                        $lcnDelta = $lcnDelta -bor (-1 -shl ($lcnSize * 8))
                    }
                    $logfileLcn = $lcnDelta  # first run, delta == absolute
                }
                $logfileLen = [System.BitConverter]::ToInt64($refLogRec, $cur + 0x30)  # real_size
                if ($logfileLen -gt 1MB) { $logfileLen = 1MB }  # cap dump
                break
            }
            $cur += $alen
        }
        if ($logfileLcn -ne $null) {
            $logfileFileOff = [int64]$logfileLcn * [int64]$refClusterSize
            $refsh.Seek($refPart.Offset + $logfileFileOff, [System.IO.SeekOrigin]::Begin) | Out-Null
            $logBuf = New-Object byte[] $logfileLen
            $null = $refsh.Read($logBuf, 0, $logfileLen)
            [System.IO.File]::WriteAllBytes("$Diag\reference-logfile.bin", $logBuf)
            "ref LogFile: lcn=$logfileLcn off=$logfileFileOff len=$logfileLen" |
                Out-File "$Diag\reference-logfile-info.txt"
        }

        # Dump reference's $AttrDef ($DATA cluster) — same parse pattern
        # as $LogFile but on MFT slot 4. ntfs.sys reports rec 4's $DATA
        # corrupt when our table differs from reference's; the byte
        # diff is the only way to find which entry fields differ.
        $refAttrRec = $refMft[(4 * 4096)..(5 * 4096 - 1)]
        $attrsOff = [System.BitConverter]::ToUInt16($refAttrRec, 0x14)
        $cur = $attrsOff
        $attrdefLcn = $null
        $attrdefLen = 4096
        while ($cur -lt $refAttrRec.Length - 8) {
            $atype = [System.BitConverter]::ToUInt32($refAttrRec, $cur)
            if ($atype -eq 0xFFFFFFFF) { break }
            $alen = [System.BitConverter]::ToUInt32($refAttrRec, $cur + 4)
            if ($alen -eq 0) { break }
            $nonRes = $refAttrRec[$cur + 8]
            if ($atype -eq 0x80 -and $nonRes -eq 1) {
                $mpOff = [System.BitConverter]::ToUInt16($refAttrRec, $cur + 0x20)
                $mp = $cur + $mpOff
                $hdr = $refAttrRec[$mp]
                $lenSize = $hdr -band 0x0F
                $lcnSize = ($hdr -shr 4) -band 0x0F
                if ($lenSize -gt 0 -and $lcnSize -gt 0) {
                    $lcnRaw = $refAttrRec[($mp + 1 + $lenSize)..($mp + $lenSize + $lcnSize)]
                    [int64]$lcnDelta = 0
                    for ($k = $lcnSize - 1; $k -ge 0; $k--) {
                        $lcnDelta = ($lcnDelta -shl 8) -bor [int64]$lcnRaw[$k]
                    }
                    $signBit = 1 -shl ($lcnSize * 8 - 1)
                    if ($lcnDelta -band $signBit) {
                        $lcnDelta = $lcnDelta -bor (-1 -shl ($lcnSize * 8))
                    }
                    $attrdefLcn = $lcnDelta
                }
                $attrdefLen = [System.BitConverter]::ToInt64($refAttrRec, $cur + 0x30)
                if ($attrdefLen -gt 65536) { $attrdefLen = 65536 }
                break
            }
            $cur += $alen
        }
        if ($attrdefLcn -ne $null) {
            $attrdefFileOff = [int64]$attrdefLcn * [int64]$refClusterSize
            $refsh.Seek($refPart.Offset + $attrdefFileOff, [System.IO.SeekOrigin]::Begin) | Out-Null
            $adBuf = New-Object byte[] $attrdefLen
            $null = $refsh.Read($adBuf, 0, $attrdefLen)
            [System.IO.File]::WriteAllBytes("$Diag\reference-attrdef.bin", $adBuf)
            "ref AttrDef: lcn=$attrdefLcn off=$attrdefFileOff len=$attrdefLen" |
                Out-File "$Diag\reference-attrdef-info.txt"
        }
    } finally { $refsh.Close() }

    # ── Stage E1: control — also chkdsk the REFERENCE volume ──────────
    # Hypothesis to test: maybe chkdsk /scan exits 13 against any
    # fresh-formatted volume (including Microsoft's own format.com
    # output), which would mean our /scan-13 result isn't a bug.
    $refLetter = $refPart.DriveLetter
    if ($refLetter) {
        $rp1 = Start-Process -FilePath chkdsk.exe -ArgumentList "${refLetter}:" `
            -NoNewWindow -PassThru -Wait `
            -RedirectStandardOutput "$Diag\reference-chkdsk-readonly.txt" `
            -RedirectStandardError "$Diag\reference-chkdsk-readonly-stderr.txt" -EA SilentlyContinue
        if ($rp1) {
            "$($rp1.ExitCode)" | Out-File "$Diag\reference-chkdsk-readonly-exit.txt" -Encoding ASCII
        }
        $rp2 = Start-Process -FilePath chkdsk.exe -ArgumentList "${refLetter}:","/scan" `
            -NoNewWindow -PassThru -Wait `
            -RedirectStandardOutput "$Diag\reference-chkdsk-scan.txt" `
            -RedirectStandardError "$Diag\reference-chkdsk-scan-stderr.txt" -EA SilentlyContinue
        if ($rp2) {
            "$($rp2.ExitCode)" | Out-File "$Diag\reference-chkdsk-scan-exit.txt" -Encoding ASCII
        }
    }
    Dismount-DiskImage -ImagePath $ReferenceVhdx | Out-Null

    # ── Stage E: chkdsk passes ─────────────────────────────────────────
    $proc1 = Start-Process -FilePath chkdsk.exe -ArgumentList "${letter}:" `
        -NoNewWindow -PassThru -Wait `
        -RedirectStandardOutput "$Diag\chkdsk-readonly.txt" `
        -RedirectStandardError "$Diag\chkdsk-readonly-stderr.txt"
    $ro = $proc1.ExitCode
    "$ro" | Out-File "$Diag\chkdsk-readonly-exit.txt" -Encoding ASCII

    $proc2 = Start-Process -FilePath chkdsk.exe -ArgumentList "${letter}:","/scan" `
        -NoNewWindow -PassThru -Wait `
        -RedirectStandardOutput "$Diag\chkdsk-scan.txt" `
        -RedirectStandardError "$Diag\chkdsk-scan-stderr.txt"
    $scan = $proc2.ExitCode
    "$scan" | Out-File "$Diag\chkdsk-scan-exit.txt" -Encoding ASCII

    # ── Stage E1.5: repeat-mount cycles (Tier-3) ───────────────────────
    # Dismount and remount the wrapper VHDX `$RemountCycles` extra times,
    # running a read-only chkdsk after each cycle. Catches bugs where
    # the volume only becomes inconsistent after multiple clean
    # dismounts (e.g. our writer leaves a $LogFile state that ntfs.sys
    # tolerates once but rejects on the third remount).
    #
    # The verdict for the scenario is still the initial $ro / $scan from
    # above; the per-cycle chkdsk exit codes land at
    #   $Diag\remount-cycle-NN-chkdsk-exit.txt
    # so an automated fix-loop can spot the cycle where things broke.
    # If any per-cycle chkdsk returns non-zero we update $ro to the
    # worst observed code so the scenario fails.
    if ($RemountCycles -gt 0) {
        for ($i = 1; $i -le $RemountCycles; $i++) {
            Dismount-DiskImage -ImagePath $Vhdx -EA SilentlyContinue | Out-Null
            Start-Sleep -Seconds 1
            Mount-DiskImage -ImagePath $Vhdx | Out-Null
            Start-Sleep -Seconds 2
            # Drive letter may have changed across remount; re-fetch.
            $vhd2 = Get-DiskImage -ImagePath $Vhdx
            $part2 = Get-Partition -DiskNumber $vhd2.Number |
                Where-Object { $_.DriveLetter } | Select-Object -First 1
            if (-not $part2) {
                "remount cycle ${i}: no drive letter assigned" |
                    Out-File "$Diag\remount-cycle-$('{0:D2}' -f $i)-error.txt" -Encoding ASCII
                continue
            }
            $cycleLetter = $part2.DriveLetter
            $procC = Start-Process -FilePath chkdsk.exe -ArgumentList "${cycleLetter}:" `
                -NoNewWindow -PassThru -Wait `
                -RedirectStandardOutput "$Diag\remount-cycle-$('{0:D2}' -f $i)-chkdsk.txt" `
                -RedirectStandardError "$Diag\remount-cycle-$('{0:D2}' -f $i)-chkdsk-stderr.txt"
            "$($procC.ExitCode)" | Out-File "$Diag\remount-cycle-$('{0:D2}' -f $i)-chkdsk-exit.txt" -Encoding ASCII
            if ($procC.ExitCode -gt $ro) {
                $ro = $procC.ExitCode
            }
        }
        # Refresh exit-code marker so the test verdict reflects worst
        # observed across all cycles.
        "$ro" | Out-File "$Diag\chkdsk-readonly-exit.txt" -Encoding ASCII
    }

    # ── Stage E2: chkdsk /F + dump the modified volume bytes ───────────
    # If /scan returns non-zero, /F often makes it pass — and what /F
    # *modifies* is exactly what was missing. Dump the post-/F MFT and
    # boot to byte-diff against the pre-/F state, so mkfs can be
    # patched to pre-emit those bytes.
    if ($scan -ne 0) {
        $proc3 = Start-Process -FilePath chkdsk.exe `
            -ArgumentList "${letter}:","/F","/X" `
            -NoNewWindow -PassThru -Wait `
            -RedirectStandardOutput "$Diag\chkdsk-fix.txt" `
            -RedirectStandardError "$Diag\chkdsk-fix-stderr.txt" -EA SilentlyContinue
        if ($proc3) {
            "$($proc3.ExitCode)" | Out-File "$Diag\chkdsk-fix-exit.txt" -Encoding ASCII
        }
        # Re-mount and dump post-/F bytes for byte-diff analysis. /F
        # may have unmounted the volume; remount.
        try {
            $vhd = Get-DiskImage -ImagePath $Vhdx -EA SilentlyContinue
            if (-not ($vhd -and $vhd.Attached)) {
                Mount-DiskImage -ImagePath $Vhdx | Out-Null
                Start-Sleep -Seconds 2
                $vhd = Get-DiskImage -ImagePath $Vhdx
            }
            $disk = Get-Disk -Number $vhd.Number
            $rawPath = "\\.\PhysicalDrive$($disk.Number)"
            $part = Get-Partition -DiskNumber $disk.Number |
                Where-Object { $_.Type -ne 'Reserved' } | Select-Object -First 1
            $fs = [System.IO.File]::Open($rawPath, [System.IO.FileMode]::Open,
                [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
            try {
                $fs.Seek($part.Offset, [System.IO.SeekOrigin]::Begin) | Out-Null
                $bootPost = New-Object byte[] 512
                $null = $fs.Read($bootPost, 0, 512)
                [System.IO.File]::WriteAllBytes("$Diag\post-fix-boot.bin", $bootPost)
                $bps = [System.BitConverter]::ToUInt16($bootPost, 0x0B)
                $spc = $bootPost[0x0D]
                $cs = [int]$bps * [int]$spc
                $mftLcn = [System.BitConverter]::ToUInt64($bootPost, 0x30)
                $mftOff = [int64]$mftLcn * [int64]$cs
                $fs.Seek($part.Offset + $mftOff, [System.IO.SeekOrigin]::Begin) | Out-Null
                $mftPost = New-Object byte[] (64KB)
                $null = $fs.Read($mftPost, 0, 64KB)
                [System.IO.File]::WriteAllBytes("$Diag\post-fix-mft-16recs.bin", $mftPost)
            } finally { $fs.Close() }
        } catch { "post-fix dump failed: $_" | Out-File "$Diag\post-fix-dump-error.txt" }

        # Run /scan again post-fix — if it returns 0, our volume was
        # just one /F away from clean.
        $proc4 = Start-Process -FilePath chkdsk.exe -ArgumentList "${letter}:","/scan" `
            -NoNewWindow -PassThru -Wait `
            -RedirectStandardOutput "$Diag\chkdsk-scan-post-fix.txt" `
            -RedirectStandardError "$Diag\chkdsk-scan-post-fix-stderr.txt" -EA SilentlyContinue
        if ($proc4) {
            "$($proc4.ExitCode)" | Out-File "$Diag\chkdsk-scan-post-fix-exit.txt" -Encoding ASCII
        }
    }

    # ── Stage F: NTFS event log (Windows kernel's view of the volume) ──
    # Filter: only events created since this scenario's mount AND only
    # those whose Message text references this scenario's volume GUID
    # OR drive letter. Drops cross-scenario noise that the prior
    # 200-event window pulled in.
    try {
        # Filter by time only (events since this scenario's mount). Don't
        # filter by GUID/letter in the message — Windows emits NTFS events
        # under several volume identifiers (HarddiskVolumeN, the GUID,
        # the drive letter) and the message text may match none of them
        # in a given form. Time-based filtering is robust.
        $events = Get-WinEvent -LogName 'System' -MaxEvents 500 -EA SilentlyContinue |
            Where-Object {
                $_.TimeCreated -ge $scenarioStart -and
                $_.ProviderName -in 'Ntfs','Microsoft-Windows-Ntfs','Disk','Volsnap','partmgr'
            }
        $events |
            Select-Object TimeCreated, ProviderName, Id, LevelDisplayName, Message |
            Format-List | Out-File "$Diag\eventlog-ntfs.txt"

        # Unique {level, id, head-of-message} triples for fast diff
        # across scenarios.
        $events | ForEach-Object {
            $msg = ($_.Message -split "`n")[0..2] -join ' ' -replace '\s+',' '
            "{0}  Id={1}  {2}" -f $_.LevelDisplayName, $_.Id, $msg.Substring(0, [Math]::Min(180, $msg.Length))
        } | Sort-Object -Unique | Out-File "$Diag\eventlog-summary.txt"
    } catch {
        "eventlog capture failed: $_" | Out-File "$Diag\eventlog-ntfs.txt"
    }

    # ── Stage F.5: extract post-Windows partition bytes back to a
    # flat image so post-Windows mac ops (e.g. mac:enumerate after
    # win:write) can run via the rust-ntfs CLI.
    #
    # Path is $Img with `.post.img` appended (e.g. nfs-foo.img.post.img).
    # Skip when Stage E2's /F unmounted the volume — the post-/F bytes
    # are already dumped under post-fix-* and the harness can fall back
    # to those if it needs the post-repair state.
    try {
        $vhdLive = Get-DiskImage -ImagePath $Vhdx -EA SilentlyContinue
        if ($vhdLive -and $vhdLive.Attached) {
            $disk2 = Get-Disk -Number $vhdLive.Number
            $part2 = Get-Partition -DiskNumber $disk2.Number |
                Where-Object { $_.Type -ne 'Reserved' } | Select-Object -First 1
            if ($part2) {
                $rawPath2 = "\\.\PhysicalDrive$($disk2.Number)"
                $postPath = "${Img}.post.img"
                $partSize = [int64]$part2.Size
                $srcFs = [System.IO.File]::Open($rawPath2, [System.IO.FileMode]::Open,
                    [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
                $dstFs = [System.IO.File]::Open($postPath, [System.IO.FileMode]::Create,
                    [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
                try {
                    $srcFs.Seek($part2.Offset, [System.IO.SeekOrigin]::Begin) | Out-Null
                    $bufSz = 16 * 1024 * 1024
                    $bf = New-Object byte[] $bufSz
                    $remaining = $partSize
                    while ($remaining -gt 0) {
                        $want = [int][System.Math]::Min([int64]$bufSz, $remaining)
                        $got = $srcFs.Read($bf, 0, $want)
                        if ($got -le 0) { break }
                        $dstFs.Write($bf, 0, $got)
                        $remaining -= $got
                    }
                    $dstFs.Flush($true)
                } finally {
                    $dstFs.Close()
                    $srcFs.Close()
                }
                "$postPath ($partSize B)" | Out-File "$Diag\post-windows-img.txt" -Encoding UTF8
            }
        }
    } catch {
        "post-windows extract failed: $_" | Out-File "$Diag\post-windows-extract-error.txt" -Encoding UTF8
    }

    # ── Stage G: emit verdict markers for the Rust harness ────────────
    Write-Output "RO_EXIT=$ro"
    Write-Output "SCAN_EXIT=$scan"
} finally {
    foreach ($v in @($Vhdx, $ReferenceVhdx)) {
        try {
            Get-DiskImage -ImagePath $v -EA SilentlyContinue |
                Where-Object Attached |
                Dismount-DiskImage -EA SilentlyContinue | Out-Null
        } catch { }
    }
}
