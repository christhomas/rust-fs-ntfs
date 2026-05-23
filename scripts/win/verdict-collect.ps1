# scripts/win/verdict-collect.ps1 — emit a JSON map of
# { scenario_name -> verdict.json contents } for every per-scenario
# diag directory under -Root (default: $env:USERPROFILE/dev/rust-fs-ntfs-matrix/diag/v2).
#
# Only scenarios whose recipes run `win-chkdsk` will have a verdict.json;
# scenarios that only exercise mac-side ops (e.g. mac-format-mac-write-*)
# are absent. matrix-baseline.sh merges this with the harness stdout to
# decorate each scenario's entry with `verdict_shape` + chkdsk exit codes.

param(
    [string]$Root = (Join-Path $env:USERPROFILE 'dev/rust-fs-ntfs-matrix/diag/v2')
)

$root = $Root

$results = [ordered]@{}
if (Test-Path $root) {
    Get-ChildItem $root -Directory | Sort-Object Name | ForEach-Object {
        $vp = Join-Path $_.FullName 'verdict.json'
        if (Test-Path $vp) {
            $results[$_.Name] = Get-Content $vp -Raw | ConvertFrom-Json
        }
    }
}
$results | ConvertTo-Json -Depth 10
