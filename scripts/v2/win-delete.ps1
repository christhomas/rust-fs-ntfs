# scripts/v2/win-delete.ps1 -- delete a single file (or empty dir) on
# the mounted NTFS volume.
#
# Mounts the .img (re-using a prior op's .vhdx if `keep_image=true` was
# set on the prior step) and removes the entry at `path` from the
# volume root.
#
# Args:
#   -ImagePath   Path on the VM to the .img file.
#   -Path        Path inside the volume to remove (e.g. "/F2.txt").
#                Forward or back slashes both work.
#   -KeepImage   `true` keeps .img + .vhdx for a follow-on op (default `false`).
#   -Diag        Directory for delete-result.txt + delete-error.txt.
#
# Exit code:
#   0 on success
#   1 if Remove-Item raised (e.g., file not found, locked)
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

# Strip a leading separator so we can join with the drive-letter root.
# Reject dot-segments (`.` / `..`) too — they normalise unpredictably
# under Windows path resolution and have no legitimate use in a recipe
# that names a file inside the freshly-mounted volume.
$relPath = $Path -replace '^[/\\]+', ''
if ($relPath -eq '' -or $relPath -match '(^|[\\/])\.{1,2}([\\/]|$)') {
    [Console]::Error.WriteLine("win-delete: -Path must name a file under the volume root with no '.' or '..' segments; got '$Path'")
    exit 2
}

New-Item -ItemType Directory -Path $Diag -Force | Out-Null

$Vhdx = Get-VhdxPathFor -ImagePath $ImagePath

try {
    $state = Initialize-VhdxFromImg -ImagePath $ImagePath -Diag $Diag
    $letter = Mount-VhdxAndGetLetter -Vhdx $state.Vhdx

    $target = "${letter}:\$relPath"
    try {
        Remove-Item -LiteralPath $target -Force
        "deleted $target" | Out-File "$Diag\delete-result.txt" -Encoding UTF8
    } catch {
        $_.Exception.Message | Out-File "$Diag\delete-error.txt" -Encoding UTF8
        throw
    }

    exit 0
} finally {
    Dismount-VhdxAndCleanup -Vhdx $Vhdx -ImagePath $ImagePath -KeepImage $KeepImageBool
}
