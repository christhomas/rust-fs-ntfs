# _verdict.ps1 -- Show-Verdict: one line saying what a check decided, and
# the tail of its report only when that answer is bad.
#
# Dot-source it: . scripts/fs-windows-test-harness/_verdict.ps1
#
# THE REPORT GOES TO THE ARTIFACT, THE VERDICT GOES TO THE LOG. chkdsk's
# full report was printed into the job log twice per run -- output the
# ntfs-windows-diag artifact already carries -- and the Windows job came to
# 1,252 lines / 106 KB (#290). A reader wants to know what it decided; a
# reader DEBUGGING wants the report, and gets the tail here and the whole
# file from the artifact.
function Show-Verdict($Label, $Path, $Exit) {
    $n = if (Test-Path $Path) { (Get-Content $Path).Count } else { 0 }
    Write-Host "$Label`: exit $Exit ($n lines) -- $Path"
    if ($Exit -ne 0 -and (Test-Path $Path)) {
        Write-Host "--- last 40 lines of $Path ---"
        Get-Content $Path -Tail 40
    }
}
