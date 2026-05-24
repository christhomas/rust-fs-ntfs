# procmon-chkdsk-trace.ps1
#
# Capture every disk + file read `chkdsk /scan` performs against a
# freshly-mkfs'd rust-ntfs volume, so we can correlate the reads with
# the byte-level diffs against a Microsoft `format.com` reference and
# identify the specific bytes that drive `chkdsk /scan` to exit 13
# ("errors queued for offline repair").
#
# Uses `wpr.exe` (Windows Performance Recorder, built into Windows
# 10/11) for ETW-based capture. Procmon's user-mode GUI app exits
# early when launched over a non-interactive SSH session (no desktop
# to attach to), so it can't be driven headlessly. wpr is purely CLI
# and runs fine in any session.
#
# What it does:
#   1. Wraps the caller-supplied `nfs.img` into a GPT-partitioned VHD
#      (Windows Mount-DiskImage requires a VHD/VHDX/ISO container with
#      a partition table, not a superfloppy raw image).
#   2. Mounts the VHD, captures the assigned drive letter.
#   3. Starts wpr capture (DiskIO + FileIO profiles).
#   4. Runs `chkdsk DRIVE:` (read-only) followed by `chkdsk DRIVE: /scan`.
#   5. Stops wpr, writes the .etl trace.
#   6. Converts the .etl to CSV via tracerpt.
#   7. Post-filters the CSV down to chkdsk.exe events on the volume.
#   8. Dismounts the VHD.
#
# Output (in -OutDir):
#   chkdsk-readonly.txt         -- stdout of `chkdsk DRIVE:`
#   chkdsk-scan.txt             -- stdout of `chkdsk DRIVE: /scan`
#   chkdsk-readonly-exit.txt    -- exit code
#   chkdsk-scan-exit.txt        -- exit code
#   chkdsk-trace.etl            -- raw ETW trace (open in WPA)
#   chkdsk-trace.csv            -- tracerpt CSV export
#   chkdsk-trace-summary.xml    -- tracerpt summary
#   chkdsk-trace-filtered.csv   -- chkdsk.exe events only
#
# Requires: vhd_tool on PATH (installed by scripts/setup-windows-vm.ps1
# via `cargo install` from antimatter-studios/rust-img-vhd), administrator
# privileges (for VHD mount + raw PhysicalDrive write + wpr's ETW session).

param(
    [Parameter(Mandatory=$true)][string]$Image,
    [Parameter(Mandatory=$true)][string]$OutDir,
    [int]$WrapperSizeMb = 384
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $Image)) {
    throw "input image not found: $Image"
}
New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
$OutDir = (Resolve-Path $OutDir).Path
$Image  = (Resolve-Path $Image).Path

# ---------------------------------------------------------------------------
# 1. Build the GPT-wrapped VHD
# ---------------------------------------------------------------------------
Write-Host "[1/8] Building wrapper VHD (${WrapperSizeMb} MiB GPT-partitioned)..."
$VhdPath = Join-Path $OutDir "wrapper.vhd"
# Clean up a leftover mount from an aborted previous run.
if (Test-Path $VhdPath) {
    Dismount-DiskImage -ImagePath $VhdPath -ErrorAction SilentlyContinue | Out-Null
    Remove-Item $VhdPath -Force
}

$wrapperSizeBytes = [int64]$WrapperSizeMb * 1MB
& vhd_tool create-fixed $VhdPath $wrapperSizeBytes | Out-Null
& fsutil sparse setflag $VhdPath 0 | Out-Null

$Vhd = Mount-DiskImage -ImagePath $VhdPath -PassThru
Start-Sleep -Seconds 2
Initialize-Disk -Number $Vhd.Number -PartitionStyle GPT
Start-Sleep -Seconds 2
$Disk = Get-Disk -Number $Vhd.Number
$Part = New-Partition -DiskNumber $Vhd.Number `
    -UseMaximumSize -AssignDriveLetter:$false

$RawBytes = [System.IO.File]::ReadAllBytes($Image)
if ($Part.Size -lt $RawBytes.Length) {
    throw "partition ($($Part.Size)) smaller than image ($($RawBytes.Length)) -- bump -WrapperSizeMb"
}
$RawPath = "\\.\PhysicalDrive$($Disk.Number)"
$Fs = [System.IO.File]::Open($RawPath, "Open", "ReadWrite", "ReadWrite")
try {
    $Fs.Seek($Part.Offset, "Begin") | Out-Null
    $Fs.Write($RawBytes, 0, $RawBytes.Length)
    $Fs.Flush($true)
} finally {
    $Fs.Close()
}
Dismount-DiskImage -ImagePath $VhdPath | Out-Null

# ---------------------------------------------------------------------------
# 2. Remount to get an assigned drive letter
# ---------------------------------------------------------------------------
Write-Host "[2/8] Remounting VHD, waiting for drive-letter assignment..."
$LettersBefore = @((Get-Volume | Where-Object { $_.DriveLetter }).DriveLetter)
$Vhd = Mount-DiskImage -ImagePath $VhdPath -PassThru
$Letter = $null
for ($i = 0; $i -lt 10; $i++) {
    Start-Sleep -Seconds 1
    $LettersAfter = @((Get-Volume | Where-Object { $_.DriveLetter }).DriveLetter)
    $New = $LettersAfter | Where-Object { $_ -notin $LettersBefore }
    if ($New) { $Letter = $New | Select-Object -First 1; break }
}
if (-not $Letter) {
    # Manual fallback -- assign one ourselves.
    $Disk = Get-Disk -Number $Vhd.Number
    $Part = Get-Partition -DiskNumber $Disk.Number |
        Where-Object { $_.Type -ne 'Reserved' } | Select-Object -First 1
    $Used = (Get-Volume | ForEach-Object { $_.DriveLetter }) +
            (Get-PSDrive -PSProvider FileSystem | ForEach-Object { $_.Name })
    foreach ($c in [char[]](68..90)) {
        if ($c -notin $Used) {
            try {
                Set-Partition -DiskNumber $Disk.Number `
                    -PartitionNumber $Part.PartitionNumber `
                    -NewDriveLetter $c -ErrorAction Stop
                $Letter = "$c"
                break
            } catch {
                # Letter may be reserved by another mount or refused by
                # the partition state; try the next letter. Surface the
                # reason at -Verbose for debugging without spamming the
                # default log when the loop ultimately succeeds.
                Write-Verbose "Set-Partition ${c}: failed: $($_.Exception.Message)"
            }
        }
    }
}
if (-not $Letter) { throw "no drive letter assigned (manual fallback failed too)" }
Write-Host "      Mounted at ${Letter}:"

# ---------------------------------------------------------------------------
# 3. Start wpr ETW capture
# ---------------------------------------------------------------------------
# Cancel any leftover session from a prior aborted run. wpr returns
# non-zero (and writes to stderr) when no session exists -- swallow.
$prevEAP = $ErrorActionPreference
$ErrorActionPreference = "Continue"
& cmd /c "wpr -cancel >NUL 2>&1"
$ErrorActionPreference = $prevEAP

Write-Host "[3/8] Starting wpr capture (DiskIO + FileIO)..."
# Multiple -start switches stack into one session. -filemode wasn't
# supported with the FileIO+Minifilter combo on this host; default
# memory-mode is fine for short captures.
$wprStart = & cmd /c "wpr -start DiskIO -start FileIO 2>&1"
if ($LASTEXITCODE -ne 0) {
    throw "wpr -start failed (exit $LASTEXITCODE): $wprStart"
}

# ---------------------------------------------------------------------------
# 4. Run chkdsk read-only + /scan
# ---------------------------------------------------------------------------
$RoLog = Join-Path $OutDir "chkdsk-readonly.txt"
$ScanLog = Join-Path $OutDir "chkdsk-scan.txt"

Write-Host "[4/8] chkdsk ${Letter}: (read-only)..."
$P1 = Start-Process -FilePath chkdsk.exe -ArgumentList "${Letter}:" `
    -NoNewWindow -PassThru -Wait `
    -RedirectStandardOutput $RoLog `
    -RedirectStandardError (Join-Path $OutDir "chkdsk-readonly-stderr.txt")
"chkdsk ${Letter}: (read-only) exit: $($P1.ExitCode)" |
    Out-File (Join-Path $OutDir "chkdsk-readonly-exit.txt")

Write-Host "      chkdsk ${Letter}: /scan..."
$P2 = Start-Process -FilePath chkdsk.exe -ArgumentList "${Letter}:","/scan" `
    -NoNewWindow -PassThru -Wait `
    -RedirectStandardOutput $ScanLog `
    -RedirectStandardError (Join-Path $OutDir "chkdsk-scan-stderr.txt")
"chkdsk ${Letter}: /scan exit: $($P2.ExitCode)" |
    Out-File (Join-Path $OutDir "chkdsk-scan-exit.txt")

# ---------------------------------------------------------------------------
# 5. Stop wpr -> .etl
# ---------------------------------------------------------------------------
$Etl = Join-Path $OutDir "chkdsk-trace.etl"
if (Test-Path $Etl) { Remove-Item $Etl -Force }
Write-Host "[5/8] Stopping wpr capture -> $Etl"
$wprStop = & wpr -stop $Etl 2>&1
if ($LASTEXITCODE -ne 0) {
    Write-Warning "wpr -stop returned $LASTEXITCODE : $wprStop"
}

# ---------------------------------------------------------------------------
# 6. Convert .etl -> .csv via tracerpt
# ---------------------------------------------------------------------------
$Csv = Join-Path $OutDir "chkdsk-trace.csv"
$SumXml = Join-Path $OutDir "chkdsk-trace-summary.xml"
if (Test-Path $Csv) { Remove-Item $Csv -Force }
if (Test-Path $SumXml) { Remove-Item $SumXml -Force }

Write-Host "[6/8] Converting .etl -> .csv via tracerpt..."
& tracerpt $Etl -o $Csv -summary $SumXml -of CSV -y 2>&1 | Out-Null

# ---------------------------------------------------------------------------
# 7. Filter CSV to chkdsk events on the drive
# ---------------------------------------------------------------------------
$FiltCsv = Join-Path $OutDir "chkdsk-trace-filtered.csv"
if (Test-Path $FiltCsv) { Remove-Item $FiltCsv -Force }

if (Test-Path $Csv) {
    Write-Host "[7/8] Filtering CSV to chkdsk events on ${Letter}:..."
    # tracerpt CSV has no fixed column for process; events vary by
    # provider. Cheap pass: keep rows mentioning chkdsk or the drive
    # letter or the raw device path. Refinement can happen on the
    # build machine after the CSV is pulled back.
    # ($filterHits, not $matches -- the latter is a PowerShell
    # automatic variable populated by the `-match` operator.)
    $filterHits = Select-String -Path $Csv -Pattern @(
        'chkdsk',
        "${Letter}:",
        "PhysicalDrive$($Disk.Number)",
        "HarddiskVolume"
    ) -SimpleMatch -CaseSensitive:$false
    $filterHits | ForEach-Object { $_.Line } | Set-Content -Path $FiltCsv

    $AllRows  = (Get-Content $Csv | Measure-Object -Line).Lines
    $FiltRows = (Get-Content $FiltCsv -ErrorAction SilentlyContinue | Measure-Object -Line).Lines
    Write-Host "      $AllRows total rows in csv, $FiltRows after filter"
} else {
    Write-Warning "tracerpt produced no CSV at $Csv"
}

# ---------------------------------------------------------------------------
# 8. Dismount
# ---------------------------------------------------------------------------
Write-Host "[8/8] Dismounting wrapper VHD..."
Dismount-DiskImage -ImagePath $VhdPath -ErrorAction SilentlyContinue | Out-Null

Write-Host ""
Write-Host "Done. Output in $OutDir"
foreach ($f in @($RoLog, $ScanLog, $Etl, $Csv, $SumXml, $FiltCsv)) {
    $present = if (Test-Path $f) { "(present)" } else { "(MISSING)" }
    Write-Host ("  {0,-40} {1}" -f (Split-Path $f -Leaf), $present)
}
