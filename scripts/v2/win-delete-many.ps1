# scripts/v2/win-delete-many.ps1 -- bulk-delete files matching a
# numbered pattern from a mounted NTFS volume in a single
# mount/dismount cycle.
#
# Pairs with win-write-many.ps1: write 256 files in one mount, delete
# every other one in one mount, chkdsk again. The single-mount shape
# is the reason for the bulk op (see win-write-many.ps1 head comment).
#
# Iteration is the half-open range `[Start, End)` stepped by `Step`,
# matching PowerShell's `for ($i = $Start; $i -lt $End; $i += $Step)`.
# Indices are zero-padded to the width of `End - 1` so they line up
# with files emitted by `win-write-many` for the same Count (e.g.
# write Count=256 -> pad width 3 -> deletes target file_000.txt,
# file_002.txt, ...).
#
# Args:
#   -ImagePath    Path on the VM to the .img file.
#   -Start        First index (inclusive, integer).
#   -End          Stop index (exclusive, integer; must be > Start).
#   -Step         Stride (positive integer).
#   -NamePattern  Volume-relative path containing the literal `{N}`
#                 placeholder (e.g. "/file_{N}.txt").
#   -KeepImage    `true` to leave .img + .vhdx for a follow-on op.
#                 Default `false`.
#   -Diag         Directory for delete-many-result.txt + delete-many-error.txt.
#
# Exit code:
#   0 on success
#   1 if any per-file delete raised (logged to delete-many-error.txt)
#   2 for arg errors

param(
    [Parameter(Mandatory=$true)] [string]$ImagePath,
    [Parameter(Mandatory=$true)] [string]$Start,
    [Parameter(Mandatory=$true)] [string]$End,
    [Parameter(Mandatory=$true)] [string]$Step,
    [Parameter(Mandatory=$true)] [string]$NamePattern,
    [Parameter(Mandatory=$true)] [string]$Diag,
    [string]$KeepImage = 'false'
)

$ErrorActionPreference = 'Stop'

. "$PSScriptRoot\_lib.ps1"

$KeepImageBool = $false
if ($KeepImage -and $KeepImage.Trim() -ne '') {
    if ($KeepImage -match '^(?i:true|1|yes)$') { $KeepImageBool = $true }
}

# Parse + validate -Start, -End, -Step.
$startInt = 0
if (-not [int]::TryParse($Start, [ref]$startInt)) {
    [Console]::Error.WriteLine("win-delete-many: invalid -Start: '$Start' (expected integer)")
    exit 2
}
if ($startInt -lt 0) {
    [Console]::Error.WriteLine("win-delete-many: -Start must be >= 0; got $startInt")
    exit 2
}
$endInt = 0
if (-not [int]::TryParse($End, [ref]$endInt)) {
    [Console]::Error.WriteLine("win-delete-many: invalid -End: '$End' (expected integer)")
    exit 2
}
if ($endInt -le $startInt) {
    [Console]::Error.WriteLine("win-delete-many: -End must be > -Start; got Start=$startInt End=$endInt")
    exit 2
}
$stepInt = 0
if (-not [int]::TryParse($Step, [ref]$stepInt)) {
    [Console]::Error.WriteLine("win-delete-many: invalid -Step: '$Step' (expected integer)")
    exit 2
}
if ($stepInt -le 0) {
    [Console]::Error.WriteLine("win-delete-many: -Step must be > 0; got $stepInt")
    exit 2
}

# Pattern must contain the `{N}` placeholder so we know where the
# index goes.
if ($NamePattern -notmatch '\{N\}') {
    [Console]::Error.WriteLine("win-delete-many: -NamePattern must contain the literal '{N}' placeholder; got '$NamePattern'")
    exit 2
}

# Pad width chosen from the largest index that could be addressed
# (End - 1) so naming lines up with win-write-many for the same Count.
$padWidth = ([string]($endInt - 1)).Length

New-Item -ItemType Directory -Path $Diag -Force | Out-Null

$Vhdx = Get-VhdxPathFor -ImagePath $ImagePath

try {
    $state  = Initialize-VhdxFromImg -ImagePath $ImagePath -Diag $Diag
    $letter = Mount-VhdxAndGetLetter -Vhdx $state.Vhdx

    $resultLines = New-Object System.Collections.Generic.List[string]
    $deleted = 0

    try {
        for ($i = $startInt; $i -lt $endInt; $i += $stepInt) {
            $idx = ([string]$i).PadLeft($padWidth, '0')
            $relRaw = $NamePattern -replace '\{N\}', $idx

            $rel = $relRaw -replace '^[/\\]+', ''
            if ($rel -eq '' -or $rel -match '(^|[\\/])\.{1,2}([\\/]|$)') {
                [Console]::Error.WriteLine("win-delete-many: -NamePattern resolved to an invalid path at index ${i}: '$relRaw'")
                exit 2
            }

            $target = "${letter}:\$rel"
            Remove-Item -LiteralPath $target -Force
            $resultLines.Add("deleted $target") | Out-Null
            $deleted++
        }

        $resultLines.Add("summary: deleted $deleted files (range [$startInt, $endInt) step $stepInt, pattern '$NamePattern', pad-width $padWidth)") | Out-Null
        $resultLines | Out-File "$Diag\delete-many-result.txt" -Encoding UTF8
    } catch {
        # Always emit the result file (even when empty) so a triage
        # agent can distinguish "op crashed before deleting anything"
        # from "op never ran". Drop a sentinel line when there's
        # nothing to report so the file's never zero-byte. Mirrors
        # the same fix in win-write-many.ps1.
        if ($resultLines.Count -eq 0) {
            $resultLines.Add("(no files deleted before failure; see delete-many-error.txt)") | Out-Null
        }
        $resultLines | Out-File "$Diag\delete-many-result.txt" -Encoding UTF8
        "failed after $deleted deletions: $($_.Exception.Message)" | Out-File "$Diag\delete-many-error.txt" -Encoding UTF8
        throw
    }

    exit 0
} finally {
    Dismount-VhdxAndCleanup -Vhdx $Vhdx -ImagePath $ImagePath -KeepImage $KeepImageBool
}
