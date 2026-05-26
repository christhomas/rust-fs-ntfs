# scripts/v2/win-mkdir.ps1 -- create a directory on the mounted NTFS
# volume.
#
# Mounts the .img (re-using a prior op's .vhd if `keep_image=true`
# was set on the prior step) and runs
# `New-Item -ItemType Directory -Path $target -Force` to create the
# directory at `path`. `-Path` (not `-LiteralPath`) because PS 5.1's
# `New-Item` doesn't accept `-LiteralPath` — the param was added in
# PS 6+. In creation context `-Path` doesn't expand wildcards anyway
# (wildcard chars become literal name components); the dot-segment
# guard below rejects the only practically dangerous shapes (`.` /
# `..`). The path is interpreted relative to the volume root.
#
# `New-Item -Force` creates parent directories automatically — the
# `deep-nesting` scenario relies on this so an 8-level path can be
# materialised in a single call rather than needing 8 separate ops.
# A recipe that wants to surface "missing parent" as an error should
# split that into per-level mkdir calls.
#
# Args:
#   -ImagePath   Path on the VM to the .img file.
#   -Path        Directory path inside the volume (e.g. "/a/b/c").
#   -KeepImage   `true` keeps .img + .vhd for a follow-on op (default `false`).
#   -Diag        Directory for mkdir-result.txt + mkdir-error.txt.
#
# Exit code:
#   0 on success
#   1 on New-Item failure (logged to mkdir-error.txt)
#   2 for arg errors

param(
    [Parameter(Mandatory=$true)] [string]$ImagePath,
    [Parameter(Mandatory=$true)] [string]$Path,
    [Parameter(Mandatory=$true)] [string]$Diag,
    [string]$KeepImage = 'false'
)

$ErrorActionPreference = 'Stop'

. "$PSScriptRoot\_lib.ps1"

$KeepImageBool = $false
if ($KeepImage -and $KeepImage.Trim() -ne '') {
    if ($KeepImage -match '^(?i:true|1|yes)$') { $KeepImageBool = $true }
}

# Strip leading separator + reject dot-segments — they normalise
# unpredictably under Windows path resolution and have no legitimate
# use in a recipe naming a directory under the freshly-mounted
# volume root.
$relPath = $Path -replace '^[/\\]+', ''
if ($relPath -eq '' -or $relPath -match '(^|[\\/])\.{1,2}([\\/]|$)') {
    [Console]::Error.WriteLine("win-mkdir: -Path must name a directory under the volume root with no '.' or '..' segments; got '$Path'")
    exit 2
}

New-Item -ItemType Directory -Path $Diag -Force | Out-Null

$Vhd = Get-VhdPathFor -ImagePath $ImagePath

try {
    $state = Initialize-VhdFromImg -ImagePath $ImagePath -Diag $Diag
    $letter = Mount-VhdAndGetLetter -Vhd $state.Vhd

    $target = "${letter}:\$relPath"
    try {
        New-Item -ItemType Directory -Path $target -Force | Out-Null
        "created directory $target" | Out-File "$Diag\mkdir-result.txt" -Encoding UTF8
    } catch {
        $_.Exception.Message | Out-File "$Diag\mkdir-error.txt" -Encoding UTF8
        throw
    }

    exit 0
} finally {
    Dismount-VhdAndCleanup -Vhd $Vhd -ImagePath $ImagePath -KeepImage $KeepImageBool
}
