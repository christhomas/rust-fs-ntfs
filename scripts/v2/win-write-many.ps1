# scripts/v2/win-write-many.ps1 -- bulk-write N zero-filled files to a
# mounted NTFS volume in a single mount/dismount cycle.
#
# Why a bulk op (vs. N x win-write steps): each single-file op pays a
# full mount + drive-letter assignment + dismount cycle (~5-10s on the
# dev VM). 256 files via single ops would be 20-40 minutes of mount
# overhead per scenario for ~64 KiB of actual user payload. This op
# does one mount, loops 256 writes, dismounts once.
#
# Body mode (size only, for now):
#   -SizeBytes <int>     each file gets that many zero bytes (>= 0)
#
# Content mode (literal-string body) is intentionally not implemented
# here yet -- no current scenario needs it. Adding it later follows the
# win-write-content shape: take a -Content string param, branch on
# which is non-empty, and ship as a separate `win-write-many-content`
# op so the cmd-shell empty-arg quirk doesn't bite (see win-write.ps1
# / [ops.win-write-content] in harness.toml for the rationale).
#
# Name pattern: -NamePattern is a path with the literal substring
# `{N}` somewhere in it (e.g. `/file_{N}.txt`). The script substitutes
# a zero-padded decimal index per iteration; the pad width is chosen
# so 0..Count-1 all share the same width (e.g. Count=256 -> 3 digits,
# `file_000.txt` .. `file_255.txt`). Fixed-width naming makes
# downstream sort-stable enumeration straightforward.
#
# Args:
#   -ImagePath    Path on the VM to the .img file.
#   -Count        Positive integer; how many files to write.
#   -NamePattern  Volume-relative path containing the literal `{N}`
#                 placeholder (e.g. "/file_{N}.txt"). Forward or back
#                 slashes both work; leading separator is stripped.
#   -SizeBytes    Number of zero bytes per file (>= 0).
#   -KeepImage    `true` to leave .img + .vhdx for a follow-on op.
#                 Default `false`.
#   -Diag         Directory for write-many-result.txt + write-many-error.txt.
#
# Exit code:
#   0 on success
#   1 if any per-file write raised (logged to write-many-error.txt;
#     the partial result is still in write-many-result.txt up to the
#     point of failure)
#   2 for arg errors (bad count, bad size, bad pattern, bad path)

param(
    [Parameter(Mandatory=$true)] [string]$ImagePath,
    [Parameter(Mandatory=$true)] [string]$Count,
    [Parameter(Mandatory=$true)] [string]$NamePattern,
    [Parameter(Mandatory=$true)] [string]$SizeBytes,
    [Parameter(Mandatory=$true)] [string]$Diag,
    [string]$KeepImage = 'false'
)

$ErrorActionPreference = 'Stop'

. "$PSScriptRoot\_lib.ps1"

$KeepImageBool = $false
if ($KeepImage -and $KeepImage.Trim() -ne '') {
    if ($KeepImage -match '^(?i:true|1|yes)$') { $KeepImageBool = $true }
}

# Parse + validate -Count.
$countInt = 0
if (-not [int]::TryParse($Count, [ref]$countInt)) {
    [Console]::Error.WriteLine("win-write-many: invalid -Count: '$Count' (expected integer)")
    exit 2
}
if ($countInt -le 0) {
    [Console]::Error.WriteLine("win-write-many: -Count must be > 0; got $countInt")
    exit 2
}

# Parse + validate -SizeBytes.
$sizeInt = 0
if (-not [int64]::TryParse($SizeBytes, [ref]$sizeInt)) {
    [Console]::Error.WriteLine("win-write-many: invalid -SizeBytes: '$SizeBytes' (expected integer)")
    exit 2
}
if ($sizeInt -lt 0) {
    [Console]::Error.WriteLine("win-write-many: -SizeBytes must be >= 0; got $sizeInt")
    exit 2
}
# byte[] allocation + FileStream.Write below both take Int32-sized
# counts. Without this guard a -SizeBytes > Int32.MaxValue would die
# at allocation time with a runtime OverflowException. Match the
# explicit-arg-error pattern used by win-modify / win-read so a
# misconfigured recipe fails with a clear stderr message instead
# of a .NET stack trace.
if ($sizeInt -gt [int]::MaxValue) {
    [Console]::Error.WriteLine("win-write-many: -SizeBytes $sizeInt exceeds [int]::MaxValue ($([int]::MaxValue)); chunked writes not yet implemented")
    exit 2
}

# Pattern must contain the `{N}` placeholder so we know where the
# index goes. Anything else is a recipe authoring error.
if ($NamePattern -notmatch '\{N\}') {
    [Console]::Error.WriteLine("win-write-many: -NamePattern must contain the literal '{N}' placeholder; got '$NamePattern'")
    exit 2
}

# Pad width chosen from the largest index we will emit (Count - 1) so
# every name shares the same width. Count=256 -> width 3 -> file_000..file_255.
$padWidth = ([string]($countInt - 1)).Length

New-Item -ItemType Directory -Path $Diag -Force | Out-Null

$Vhdx = Get-VhdxPathFor -ImagePath $ImagePath

try {
    $state  = Initialize-VhdxFromImg -ImagePath $ImagePath -Diag $Diag
    $letter = Mount-VhdxAndGetLetter -Vhdx $state.Vhdx

    # Pre-allocate a single zero buffer + reuse it across all files;
    # each file is a single Write call when SizeBytes is small. For a
    # (currently hypothetical) very large per-file size we'd want the
    # 64 KiB chunked-stream loop from win-write.ps1, but the only
    # consumer today is 16-byte files.
    $chunk = New-Object byte[] ([Math]::Max($sizeInt, 1))

    $resultLines = New-Object System.Collections.Generic.List[string]
    $written = 0

    try {
        for ($i = 0; $i -lt $countInt; $i++) {
            $idx = ([string]$i).PadLeft($padWidth, '0')
            $relRaw = $NamePattern -replace '\{N\}', $idx

            # Strip leading separator + reject dot-segments. Same shape
            # as win-rename's Resolve-RelPath. Validation happens per
            # iteration because the pattern -> path substitution can
            # in principle produce a `..` (e.g. someone writes
            # `/foo/.{N}.txt` with N starting at 0 producing `/foo/.0.txt`
            # -- fine -- but a sloppier pattern could land on `..`).
            $rel = $relRaw -replace '^[/\\]+', ''
            if ($rel -eq '' -or $rel -match '(^|[\\/])\.{1,2}([\\/]|$)') {
                [Console]::Error.WriteLine("win-write-many: -NamePattern resolved to an invalid path at index ${i}: '$relRaw'")
                exit 2
            }

            $target = "${letter}:\$rel"
            $fs = [System.IO.File]::Open($target, [System.IO.FileMode]::Create,
                [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
            try {
                if ($sizeInt -gt 0) {
                    $fs.Write($chunk, 0, [int]$sizeInt)
                }
                $fs.Flush($true)
            } finally { $fs.Close() }

            $resultLines.Add("wrote $sizeInt zero bytes to $target") | Out-Null
            $written++
        }

        $resultLines.Add("summary: wrote $written files of $sizeInt bytes (pattern '$NamePattern', pad-width $padWidth)") | Out-Null
        $resultLines | Out-File "$Diag\write-many-result.txt" -Encoding UTF8
    } catch {
        # Persist whatever we got so the failure point is visible in diag.
        if ($resultLines.Count -gt 0) {
            $resultLines | Out-File "$Diag\write-many-result.txt" -Encoding UTF8
        }
        "failed after $written / $countInt files: $($_.Exception.Message)" | Out-File "$Diag\write-many-error.txt" -Encoding UTF8
        throw
    }

    exit 0
} finally {
    Dismount-VhdxAndCleanup -Vhdx $Vhdx -ImagePath $ImagePath -KeepImage $KeepImageBool
}
