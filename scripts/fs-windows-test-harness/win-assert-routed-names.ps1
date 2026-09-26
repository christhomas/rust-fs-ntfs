param(
    [Parameter(Mandatory=$true)] [string]$Diag,
    [Parameter(Mandatory=$true)] [string]$Directory,
    [Parameter(Mandatory=$true)] [int]$Count
)

$ErrorActionPreference = 'Stop'
if (Test-Path -LiteralPath "$Diag\enumerate-error.txt") {
    throw 'Windows enumeration reported an error'
}
$listing = "$Diag\enumerate.txt"
if (-not (Test-Path -LiteralPath $listing)) {
    throw 'Windows enumeration produced no listing'
}
$paths = @(Get-Content -LiteralPath $listing)
for ($i = 0; $i -lt $Count; $i++) {
    $name = 'f_{0:D4}.txt' -f $i
    $suffix = '\' + $Directory + '\' + $name
    if (-not @($paths | Where-Object { $_.EndsWith($suffix, [StringComparison]::OrdinalIgnoreCase) }).Count) {
        throw "Windows could not enumerate $Directory/$name"
    }
}
