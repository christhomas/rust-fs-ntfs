# scripts/v2/win-enumerate.ps1 -- list the files/dirs on a mounted
# NTFS volume.
#
# Mounts the .img (creating a fresh VHD wrapper if there isn't one
# already from a prior op in the same scenario) and walks the volume
# root recursively, writing the listing to <Diag>\enumerate.txt for
# the harness verdict layer + a future v2 verdict-shape system to
# consume.
#
# Args:
#   -ImagePath   Path on the VM to the .img file. If a .vhd wrapper
#                of the same basename already exists (left behind by a
#                prior win-* op with KeepImage=true), it's reused;
#                otherwise it's built from the .img.
#   -KeepImage   If `true`, the .img and .vhd are left in place after
#                this op so a follow-on op can mount them. Default
#                `false` (this op is the cleanup point for the
#                scenario).
#   -Diag        Directory to write diag artefacts into:
#                  enumerate.txt        - one line per entry, full path
#                  enumerate-error.txt  - if Get-ChildItem raised
#                  wrapper-create.txt   - vhd_tool output (if first op)
#
# Exit code:
#   0 always for now (matches v1's win:enumerate semantics: it's an
#     observation, not a verdict). Future verdict shapes (e.g.
#     "expect this exact set of paths") will gate this.

param(
    [Parameter(Mandatory=$true)] [string]$ImagePath,
    [Parameter(Mandatory=$true)] [string]$Diag,
    [string]$KeepImage = 'false'
)

$ErrorActionPreference = 'Stop'

. "$PSScriptRoot\_lib.ps1"

$KeepImageBool = $false
if ($KeepImage -and $KeepImage.Trim() -ne '') {
    if ($KeepImage -match '^(?i:true|1|yes)$') { $KeepImageBool = $true }
}

New-Item -ItemType Directory -Path $Diag -Force | Out-Null

$Vhd = Get-VhdPathFor -ImagePath $ImagePath

try {
    $state = Initialize-VhdFromImg -ImagePath $ImagePath -Diag $Diag
    $letter = Mount-VhdAndGetLetter -Vhd $state.Vhd

    try {
        # `-Force` so System / Hidden NTFS metadata files are surfaced
        # alongside user content; v1's enumerate verdict comparisons
        # historically included them.
        Get-ChildItem -LiteralPath "${letter}:\" -Recurse -Force -EA SilentlyContinue |
            Select-Object -ExpandProperty FullName |
            Out-File "$Diag\enumerate.txt" -Encoding UTF8
    } catch {
        # Don't rethrow — the script's contract (and harness's
        # `expect_exit = 0`) is "always exit 0; enumeration is an
        # observation, not a verdict". Log the failure for triage and
        # let the run continue. Future verdict shapes that gate on the
        # listing's content can flip this.
        $_.Exception.Message | Out-File "$Diag\enumerate-error.txt" -Encoding UTF8
    }

    exit 0
} finally {
    Dismount-VhdAndCleanup -Vhd $Vhd -ImagePath $ImagePath -KeepImage $KeepImageBool
}
