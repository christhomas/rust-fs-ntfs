# scripts/v2/win-rename.ps1 -- rename a single file on the mounted
# NTFS volume.
#
# Mounts the .img (re-using a prior op's .vhd if `keep_image=true`
# was set on the prior step) and runs `Move-Item -LiteralPath -Force`
# to rename `from-path` to `to-path`. Both paths are interpreted
# relative to the volume root; their parent directories must already
# exist (this op doesn't mkdir).
#
# Args:
#   -ImagePath   Path on the VM to the .img file.
#   -FromPath    Source path inside the volume (e.g. "/a.txt").
#   -ToPath      Destination path inside the volume (e.g. "/b.txt").
#   -KeepImage   `true` keeps .img + .vhd for a follow-on op (default `false`).
#   -Diag        Directory for rename-result.txt + rename-error.txt.
#
# Exit code:
#   0 on success
#   1 on Move-Item failure (logged to rename-error.txt)
#   2 for arg errors

param(
    [Parameter(Mandatory=$true)] [string]$ImagePath,
    [Parameter(Mandatory=$true)] [string]$FromPath,
    [Parameter(Mandatory=$true)] [string]$ToPath,
    [Parameter(Mandatory=$true)] [string]$Diag,
    [string]$KeepImage = 'false'
)

$ErrorActionPreference = 'Stop'

. "$PSScriptRoot\_lib.ps1"

$KeepImageBool = $false
if ($KeepImage -and $KeepImage.Trim() -ne '') {
    if ($KeepImage -match '^(?i:true|1|yes)$') { $KeepImageBool = $true }
}

# Strip leading separator + reject dot-segments on both paths — they
# normalise unpredictably under Windows path resolution and have no
# legitimate use in a recipe naming a file inside the freshly-mounted
# volume.
function Resolve-RelPath([string]$raw, [string]$argName) {
    $rel = $raw -replace '^[/\\]+', ''
    if ($rel -eq '' -or $rel -match '(^|[\\/])\.{1,2}([\\/]|$)') {
        [Console]::Error.WriteLine("win-rename: -$argName must name a file under the volume root with no '.' or '..' segments; got '$raw'")
        exit 2
    }
    return $rel
}

$relFrom = Resolve-RelPath $FromPath 'FromPath'
$relTo   = Resolve-RelPath $ToPath   'ToPath'

New-Item -ItemType Directory -Path $Diag -Force | Out-Null

$Vhd = Get-VhdPathFor -ImagePath $ImagePath

try {
    $state = Initialize-VhdFromImg -ImagePath $ImagePath -Diag $Diag
    $letter = Mount-VhdAndGetLetter -Vhd $state.Vhd

    $src = "${letter}:\$relFrom"
    $dst = "${letter}:\$relTo"
    try {
        Move-Item -LiteralPath $src -Destination $dst -Force
        "renamed $src -> $dst" | Out-File "$Diag\rename-result.txt" -Encoding UTF8
    } catch {
        $_.Exception.Message | Out-File "$Diag\rename-error.txt" -Encoding UTF8
        throw
    }

    exit 0
} finally {
    Dismount-VhdAndCleanup -Vhd $Vhd -ImagePath $ImagePath -KeepImage $KeepImageBool
}
