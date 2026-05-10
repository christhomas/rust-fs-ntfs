# scripts/v2/win-modify.ps1 -- write a deterministic byte pattern at a
# specific offset inside an existing file on the mounted NTFS volume.
#
# Mounts the .img (re-using a prior op's .vhd if `keep_image=true` was
# set on the prior step), opens the target file at `<letter>:\<path>`
# for ReadWrite, seeks to `Offset`, writes `Length` bytes of a
# deterministic 0xFF pattern, flushes, and closes. The 0xFF pattern is
# chosen so a follow-on win-read can verify the modified region against
# a known-non-zero value (the rest of the file, written by win-write-size,
# is zeros).
#
# This op is the v2 equivalent of v1's `win:modify(path offset=N len=M)`.
# It exists alongside win-write-size / win-write-content because writing
# at an offset into an existing non-resident $DATA exercises a different
# code path (run-list rewrite when a single run is split) than overwriting
# the file from offset 0.
#
# Args:
#   -ImagePath  Path on the VM to the .img file.
#   -Path       File path inside the volume (e.g. "/big.bin"). Forward or
#               back slashes both work; leading separator is stripped.
#   -Offset     Byte offset inside the file to start writing at (integer
#               as string; >= 0). The file must already be at least
#               Offset+Length bytes long.
#   -Length     Number of bytes to write (integer as string; >= 0). Zero
#               is a valid no-op (logged but no I/O).
#   -KeepImage  `true` to leave .img + .vhd for a follow-on op. Default `false`.
#   -Diag       Directory for modify-result.txt + modify-error.txt.
#
# Exit code:
#   0 on success
#   1 on write failure (logged to modify-error.txt)
#   2 for arg errors (bad path, non-integer Offset/Length, negative values)

param(
    [Parameter(Mandatory=$true)] [string]$ImagePath,
    [Parameter(Mandatory=$true)] [string]$Path,
    [Parameter(Mandatory=$true)] [string]$Offset,
    [Parameter(Mandatory=$true)] [string]$Length,
    [Parameter(Mandatory=$true)] [string]$Diag,
    [string]$KeepImage = 'false'
)

$ErrorActionPreference = 'Stop'

. "$PSScriptRoot\_lib.ps1"

$KeepImageBool = $false
if ($KeepImage -and $KeepImage.Trim() -ne '') {
    if ($KeepImage -match '^(?i:true|1|yes)$') { $KeepImageBool = $true }
}

# Strip a leading separator + reject dot-segments. Same shape as
# win-write / win-rename / win-delete: dot-segments normalise
# unpredictably under Windows path resolution and have no legitimate
# use in a recipe naming a file inside the freshly-mounted volume.
$relPath = $Path -replace '^[/\\]+', ''
if ($relPath -eq '' -or $relPath -match '(^|[\\/])\.{1,2}([\\/]|$)') {
    [Console]::Error.WriteLine("win-modify: -Path must name a file under the volume root with no '.' or '..' segments; got '$Path'")
    exit 2
}

# Parse Offset + Length as int64 (file lengths can exceed Int32 even
# though this op writes a small window).
$offsetInt = 0
if (-not [int64]::TryParse($Offset, [ref]$offsetInt)) {
    [Console]::Error.WriteLine("win-modify: invalid -Offset: '$Offset' (expected integer)")
    exit 2
}
if ($offsetInt -lt 0) {
    [Console]::Error.WriteLine("win-modify: -Offset must be >= 0; got $offsetInt")
    exit 2
}

$lengthInt = 0
if (-not [int64]::TryParse($Length, [ref]$lengthInt)) {
    [Console]::Error.WriteLine("win-modify: invalid -Length: '$Length' (expected integer)")
    exit 2
}
if ($lengthInt -lt 0) {
    [Console]::Error.WriteLine("win-modify: -Length must be >= 0; got $lengthInt")
    exit 2
}

New-Item -ItemType Directory -Path $Diag -Force | Out-Null

$Vhd = Get-VhdPathFor -ImagePath $ImagePath

try {
    $state = Initialize-VhdFromImg -ImagePath $ImagePath -Diag $Diag
    $letter = Mount-VhdAndGetLetter -Vhd $state.Vhd

    $target = "${letter}:\$relPath"
    try {
        if ($lengthInt -eq 0) {
            "no-op: -Length is 0; nothing written to $target at offset $offsetInt" |
                Out-File "$Diag\modify-result.txt" -Encoding UTF8
        } else {
            # Build the 0xFF pattern buffer once. Length is bounded by
            # the recipe (typical use: 4 KiB), so a single byte[] is
            # fine. If a future recipe needs >2 GiB modifications,
            # switch to chunked writes the way win-write does.
            if ($lengthInt -gt [int]::MaxValue) {
                [Console]::Error.WriteLine("win-modify: -Length $lengthInt exceeds the single-buffer cap; chunked writes not yet implemented")
                exit 2
            }
            $buf = New-Object byte[] ([int]$lengthInt)
            for ($i = 0; $i -lt $buf.Length; $i++) { $buf[$i] = 0xFF }

            $fs = [System.IO.File]::Open($target, [System.IO.FileMode]::Open,
                [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::None)
            try {
                # Enforce the documented contract: target must already
                # be at least Offset+Length bytes long. Without this
                # check, a too-large Offset+Length silently *extends*
                # the file (FileMode::Open + FileAccess::ReadWrite
                # allows growth past current EOF). That hides recipe
                # errors — the modification would land somewhere the
                # author didn't expect, and a follow-on win-read
                # against the post-write offset would see writes that
                # weren't supposed to be there.
                if ($offsetInt -gt ([int64]::MaxValue - $lengthInt)) {
                    [Console]::Error.WriteLine("win-modify: -Offset + -Length overflows Int64 (offset=$offsetInt, len=$lengthInt)")
                    exit 2
                }
                $endExclusive = $offsetInt + $lengthInt
                if ($endExclusive -gt $fs.Length) {
                    [Console]::Error.WriteLine("win-modify: target file too small (file length=$($fs.Length), need >= $endExclusive for offset=$offsetInt + len=$lengthInt)")
                    exit 2
                }
                $fs.Seek($offsetInt, [System.IO.SeekOrigin]::Begin) | Out-Null
                $fs.Write($buf, 0, $buf.Length)
                $fs.Flush($true)
            } finally { $fs.Close() }

            "wrote $lengthInt bytes at offset $offsetInt to $target with pattern 0xFF" |
                Out-File "$Diag\modify-result.txt" -Encoding UTF8
        }
    } catch {
        $_.Exception.Message | Out-File "$Diag\modify-error.txt" -Encoding UTF8
        throw
    }

    exit 0
} finally {
    Dismount-VhdAndCleanup -Vhd $Vhd -ImagePath $ImagePath -KeepImage $KeepImageBool
}
