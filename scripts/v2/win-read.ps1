# scripts/v2/win-read.ps1 -- read a byte range from a file on the
# mounted NTFS volume and dump it to a diag artefact.
#
# Mounts the .img (re-using a prior op's .vhdx if `keep_image=true` was
# set on the prior step), opens the target file at `<letter>:\<path>`
# for Read, seeks to `Offset`, reads `Length` bytes, writes the raw
# bytes to <Diag>\read-bytes.bin and a hex-dump summary to
# <Diag>\read-bytes.txt.
#
# This op is the v2 equivalent of v1's `win:read(path)`. It is
# observation-only today (always exits 0 on a successful read) -- a
# future verdict shape can gate against the captured bytes (e.g.
# "expect this exact pattern at this offset"). For the
# write-modify-chkdsk-read recipe, the captured bytes let us verify
# manually that the modify op landed at the right offset.
#
# Args:
#   -ImagePath  Path on the VM to the .img file.
#   -Path       File path inside the volume (e.g. "/big.bin"). Forward or
#               back slashes both work; leading separator is stripped.
#   -Offset     Byte offset inside the file to start reading from
#               (integer as string; >= 0).
#   -Length     Number of bytes to read (integer as string; >= 0). A
#               short read (file ends before Offset+Length) is captured
#               truthfully -- the artefact reflects what we got, not
#               what we asked for.
#   -KeepImage  `true` to leave .img + .vhdx for a follow-on op. Default `false`.
#   -Diag       Directory for read-bytes.bin + read-bytes.txt + read-error.txt.
#
# Exit code:
#   0 on a successful read (any byte count, including 0)
#   1 on read failure (logged to read-error.txt)
#   2 for arg errors

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

# Strip leading separator + reject dot-segments. Same shape as the
# other path-validating ops (win-write / win-rename / win-delete /
# win-modify).
$relPath = $Path -replace '^[/\\]+', ''
if ($relPath -eq '' -or $relPath -match '(^|[\\/])\.{1,2}([\\/]|$)') {
    [Console]::Error.WriteLine("win-read: -Path must name a file under the volume root with no '.' or '..' segments; got '$Path'")
    exit 2
}

$offsetInt = 0
if (-not [int64]::TryParse($Offset, [ref]$offsetInt)) {
    [Console]::Error.WriteLine("win-read: invalid -Offset: '$Offset' (expected integer)")
    exit 2
}
if ($offsetInt -lt 0) {
    [Console]::Error.WriteLine("win-read: -Offset must be >= 0; got $offsetInt")
    exit 2
}

$lengthInt = 0
if (-not [int64]::TryParse($Length, [ref]$lengthInt)) {
    [Console]::Error.WriteLine("win-read: invalid -Length: '$Length' (expected integer)")
    exit 2
}
if ($lengthInt -lt 0) {
    [Console]::Error.WriteLine("win-read: -Length must be >= 0; got $lengthInt")
    exit 2
}
if ($lengthInt -gt [int]::MaxValue) {
    [Console]::Error.WriteLine("win-read: -Length $lengthInt exceeds the single-buffer cap; chunked reads not yet implemented")
    exit 2
}

New-Item -ItemType Directory -Path $Diag -Force | Out-Null

$Vhdx = Get-VhdxPathFor -ImagePath $ImagePath

try {
    $state = Initialize-VhdxFromImg -ImagePath $ImagePath -Diag $Diag
    $letter = Mount-VhdxAndGetLetter -Vhdx $state.Vhdx

    $target = "${letter}:\$relPath"
    try {
        $buf = New-Object byte[] ([int]$lengthInt)
        $bytesRead = 0
        if ($lengthInt -gt 0) {
            $fs = [System.IO.File]::Open($target, [System.IO.FileMode]::Open,
                [System.IO.FileAccess]::Read, [System.IO.FileShare]::Read)
            try {
                $fs.Seek($offsetInt, [System.IO.SeekOrigin]::Begin) | Out-Null
                # Loop until we've filled the requested window or hit
                # EOF -- a single .Read() can short-read even when more
                # bytes are available.
                $remaining = [int]$lengthInt
                $writePos  = 0
                while ($remaining -gt 0) {
                    $n = $fs.Read($buf, $writePos, $remaining)
                    if ($n -le 0) { break }
                    $bytesRead += $n
                    $writePos  += $n
                    $remaining -= $n
                }
            } finally { $fs.Close() }
        }

        # Trim the buffer to the actual bytes read so the .bin artefact
        # reflects truth (a short read at EOF would otherwise leave
        # trailing zero padding that looks like real data).
        $captured = New-Object byte[] $bytesRead
        if ($bytesRead -gt 0) {
            [Array]::Copy($buf, 0, $captured, 0, $bytesRead)
        }
        [System.IO.File]::WriteAllBytes("$Diag\read-bytes.bin", $captured)

        # Build a compact hex-dump summary (first/last 64 bytes + a
        # one-line header). For the modify scenario the modified region
        # is 4 KiB of 0xFF, so the head dump shows "FF FF FF ..." which
        # is enough to eyeball the verdict; the .bin is there for any
        # downstream byte-exact check.
        $headLen = [Math]::Min(64, $bytesRead)
        $headHex = ''
        if ($headLen -gt 0) {
            $headHex = ($captured[0..($headLen - 1)] | ForEach-Object { '{0:X2}' -f $_ }) -join ' '
        }
        $tailHex = ''
        if ($bytesRead -gt 64) {
            $tailStart = $bytesRead - [Math]::Min(64, $bytesRead)
            $tailHex = ($captured[$tailStart..($bytesRead - 1)] | ForEach-Object { '{0:X2}' -f $_ }) -join ' '
        }

        $lines = @()
        $lines += "read $bytesRead bytes (requested $lengthInt) at offset $offsetInt from $target"
        $lines += "head[0..$($headLen - 1)]: $headHex"
        if ($tailHex -ne '' -and $bytesRead -gt 64) {
            $lines += "tail[$($bytesRead - [Math]::Min(64, $bytesRead))..$($bytesRead - 1)]: $tailHex"
        }
        ($lines -join "`r`n") | Out-File "$Diag\read-bytes.txt" -Encoding UTF8
    } catch {
        $_.Exception.Message | Out-File "$Diag\read-error.txt" -Encoding UTF8
        throw
    }

    exit 0
} finally {
    Dismount-VhdxAndCleanup -Vhdx $Vhdx -ImagePath $ImagePath -KeepImage $KeepImageBool
}
