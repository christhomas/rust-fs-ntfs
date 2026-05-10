# scripts/v2/win-chkdsk.ps1 -- minimal win-side helper for v2 recipes.
#
# A v2 alternative to scripts/run-scenario.ps1's chkdsk lifecycle,
# trimmed to just what's needed when the v2 dispatcher invokes it
# per-step over SSH. The full v1 driver still exists; this script is
# the per-op replacement chain that retires it.
#
# Wraps a host-side .img (already shipped to the VM via the harness's
# built-in `ship-to-vm` op) into a temporary VHD, mounts it on
# Windows, runs chkdsk against the resulting drive letter with the
# requested modes, dismounts, and cleans up.
#
# Args:
#   -ImagePath   Path on the VM to the .img file (typically
#                <vm.workdir>/<scenario.image>).
#   -Modes       Comma-separated list of chkdsk passes to run, in
#                order: readonly, /scan, /spotfix, /F, /F /scan.
#                Empty / absent => run readonly only (matches the
#                default `mac:format -> win:chkdsk` shape).
#   -VerdictShape  `Clean` (default) or `RepairRequired`. See the
#                  comment block above the chkdsk loop for the gating
#                  rules.
#   -KeepImage   If `true` (string, from `{step.keep_image?}`), the
#                .img and .vhd are left in place on the VM after this
#                op completes so a follow-on win-* op can mount them.
#                Default `false` matches the single-win-op recipe shape.
#                The final win-* op in a multi-op recipe must omit
#                this flag (or pass `false`) so cleanup runs.
#   -Diag        Directory to write diag artefacts into:
#                  chkdsk-<mode>.txt        - chkdsk's stdout
#                  chkdsk-<mode>-exit.txt   - exit code marker
#                  mount-eventlog.txt       - Disk/Ntfs/partmgr events
#                  wrapper-create.txt       - vhd_tool output
#                  verdict.json             - final pass/fail summary
#
# Exit code:
#   0 if every chkdsk mode exited 0 (Clean) or the RepairRequired
#     verdict logic returned passed=true
#   1 otherwise; per-mode exit codes are in <Diag>/chkdsk-*-exit.txt
#   2 for config errors (bad -VerdictShape, missing /scan in
#     RepairRequired modes)
#
# Phase 1e (done): this script invokes `vhd_tool create-fixed` from
# antimatter-studios/rust-img-vhd to wrap the .img into a VHD before
# mounting (replaced the prior qemu-img dep). The rest of the
# lifecycle (mount, initialize, dd, chkdsk) is unchanged.

param(
    [Parameter(Mandatory=$true)] [string]$ImagePath,
    [string]$Modes = "readonly",
    [Parameter(Mandatory=$true)] [string]$Diag,
    [string]$VerdictShape = 'Clean',
    [string]$KeepImage = 'false'
)

$ErrorActionPreference = 'Stop'

. "$PSScriptRoot\_lib.ps1"

# Accept empty (from `{step.verdict_shape?}` substitution when omitted)
# as the Clean default so callers don't have to spell it out everywhere.
if (-not $VerdictShape -or $VerdictShape.Trim() -eq '') {
    $VerdictShape = 'Clean'
}
if ($VerdictShape -ne 'Clean' -and $VerdictShape -ne 'RepairRequired') {
    Write-Error "invalid -VerdictShape: '$VerdictShape' (expected Clean or RepairRequired)"
    exit 2
}

# Same trick for KeepImage: empty -> false. Accept any case-insensitive
# truthy string so recipes can write "True" without surprises.
$KeepImageBool = $false
if ($KeepImage -and $KeepImage.Trim() -ne '') {
    if ($KeepImage -match '^(?i:true|1|yes)$') { $KeepImageBool = $true }
}

New-Item -ItemType Directory -Path $Diag -Force | Out-Null

$startTime = Get-Date
$state = $null
$Vhd = Get-VhdPathFor -ImagePath $ImagePath

try {
    $state = Initialize-VhdFromImg -ImagePath $ImagePath -Diag $Diag
    $letter = Mount-VhdAndGetLetter -Vhd $state.Vhd

    # ── chkdsk passes ─────────────────────────────────────────────
    #
    # Pass/fail rules — match v1's tests/matrix.rs `VerdictShape`:
    #
    #   Clean (default):
    #     - readonly:  must exit 0
    #     - /scan:     0, 11, 13 are all "ok"
    #         - 0  = clean
    #         - 11 = frs.cxx 60f ceiling (known v1 technical debt)
    #         - 13 = VSS / shadow-copy infra flake on tiny volumes
    #
    #   RepairRequired:
    #     - run the listed modes first (capture exits, don't gate);
    #       a `set-dirty` scenario expects the pre-/F /scan to return
    #       non-zero (proving the volume was actually dirty)
    #     - run /F (capture as `fix_exit`)
    #     - run post-/F /scan (capture as `post_scan_exit`)
    #     - verdict: pre_scan != 0 AND fix_exit == 0 AND post_scan_exit == 0
    $rawExits = @{}      # diag-key -> exit code (for diag inspection)
    function Invoke-ChkdskMode([string]$mode, [string]$letter, [string]$diag, [string]$labelSuffix = '') {
        $modeFile = ($mode -replace '[/\\ ]', '-') + $labelSuffix
        $log = "$diag\chkdsk-$modeFile.txt"
        $exitFile = "$diag\chkdsk-$modeFile-exit.txt"
        $argsList = @("${letter}:")
        if ($mode -ne "readonly") {
            $argsList += $mode -split ' '
        }
        $proc = Start-Process -FilePath chkdsk -ArgumentList $argsList -NoNewWindow -PassThru -Wait -RedirectStandardOutput $log
        "$($proc.ExitCode)" | Out-File $exitFile -Encoding ASCII
        return $proc.ExitCode
    }

    if ($VerdictShape -eq 'Clean') {
        $passed = $true
        foreach ($mode in $Modes.Split(',') | ForEach-Object { $_.Trim() } | Where-Object { $_ }) {
            $exit = Invoke-ChkdskMode -mode $mode -letter $letter -diag $Diag
            $rawExits[$mode] = $exit
            if ($mode -eq 'readonly') {
                if ($exit -ne 0) { $passed = $false }
            } else {
                if ($exit -ne 0 -and $exit -ne 11 -and $exit -ne 13) {
                    $passed = $false
                }
            }
        }
        @{
            passed = $passed
            verdict_shape = 'clean'
            exits = $rawExits
        } | ConvertTo-Json -Compress | Out-File "$Diag\verdict.json" -Encoding ASCII
    } else {
        # RepairRequired
        $preScanExit = $null
        foreach ($mode in $Modes.Split(',') | ForEach-Object { $_.Trim() } | Where-Object { $_ }) {
            $exit = Invoke-ChkdskMode -mode $mode -letter $letter -diag $Diag
            $rawExits[$mode] = $exit
            if ($mode -eq '/scan') { $preScanExit = $exit }
        }
        # The verdict gates on `pre_scan != 0` (proves the volume was
        # actually dirty), so `/scan` must be in -Modes. Surface a clear
        # config error rather than letting the verdict silently fail with
        # `pre_scan == $null`. We bypass `Write-Error` here because
        # `$ErrorActionPreference = 'Stop'` would convert it to a
        # terminating error, propagate to the outer `try`/`finally`, and
        # exit 1 — masking the intentional `exit 2` for "config error".
        if ($null -eq $preScanExit) {
            [Console]::Error.WriteLine("RepairRequired requires '/scan' in -Modes; got -Modes '$Modes'")
            exit 2
        }
        # /F + post-/F /scan run regardless; their exits drive the
        # verdict alongside pre_scan. `/F /X` matches v1's run-scenario.ps1
        # — `/X` forces an exclusive dismount before the fix so chkdsk
        # doesn't hang on a "do you want to dismount?" prompt that we
        # can't answer non-interactively. The post-scan reuses the
        # `/scan` chkdsk arg but lands in `chkdsk--scan-post.txt` so
        # the pre/post logs are distinct.
        $fixExit = Invoke-ChkdskMode -mode '/F /X' -letter $letter -diag $Diag
        $rawExits['/F /X'] = $fixExit
        $postScanExit = Invoke-ChkdskMode -mode '/scan' -letter $letter -diag $Diag -labelSuffix '-post'
        $rawExits['/scan-post'] = $postScanExit
        $passed = ($null -ne $preScanExit) -and ($preScanExit -ne 0) `
                  -and ($fixExit -eq 0) -and ($postScanExit -eq 0)
        @{
            passed = $passed
            verdict_shape = 'repair-required'
            exits = $rawExits
            pre_scan_exit = $preScanExit
            fix_exit = $fixExit
            post_scan_exit = $postScanExit
        } | ConvertTo-Json -Compress | Out-File "$Diag\verdict.json" -Encoding ASCII
    }

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
    Dismount-VhdAndCleanup -Vhd $Vhd -ImagePath $ImagePath -KeepImage $KeepImageBool
}
