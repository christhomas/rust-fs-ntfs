# scripts/v2/win-write.ps1 -- write a single file to a mounted NTFS
# volume.
#
# Mounts the .img (creating a fresh VHDX wrapper if there isn't one
# from a prior op) and writes a single file at the requested path.
#
# Two body modes:
#   -Content "<string>"     write the literal UTF-8 bytes of <string>
#                           (text scenarios: tiny.txt='hello world')
#   -SizeBytes <int>        write that many zero bytes
#                           (binary scenarios: medium.bin=4KB, big.bin=4MiB)
#
# Exactly one of -Content or -SizeBytes must be supplied; if both are
# given, -Content wins. -SizeBytes streams in 64 KiB chunks so files
# above the .NET 2 GiB byte[] cap work.
#
# The path argument is interpreted relative to the volume root; if it
# starts with `/` or `\` the leading separator is stripped. Parent
# directories must already exist (this op doesn't mkdir).
#
# Args:
#   -ImagePath  Path on the VM to the .img file.
#   -Path       File path inside the volume, e.g. "/tiny.txt".
#   -Content    Literal string to write (UTF-8 bytes, no trailing newline).
#   -SizeBytes  Number of zero bytes to write. Mutually exclusive with -Content.
#   -KeepImage  `true` to leave .img + .vhdx for a follow-on op. Default `false`.
#   -Diag       Directory for write-result.txt + wrapper-create.txt.
#
# Exit code:
#   0 on success
#   1 on write failure (logged to write-error.txt)
#   2 for arg errors (missing -Content and -SizeBytes both, or bad path)

param(
    [Parameter(Mandatory=$true)] [string]$ImagePath,
    [Parameter(Mandatory=$true)] [string]$Path,
    [string]$Content = '',
    [string]$SizeBytes = '',
    [Parameter(Mandatory=$true)] [string]$Diag,
    [string]$KeepImage = 'false'
)

$ErrorActionPreference = 'Stop'

. "$PSScriptRoot\_lib.ps1"

$KeepImageBool = $false
if ($KeepImage -and $KeepImage.Trim() -ne '') {
    if ($KeepImage -match '^(?i:true|1|yes)$') { $KeepImageBool = $true }
}

# Decide which body mode we're in. -Content takes precedence; only fall
# through to -SizeBytes if -Content is empty/absent. Both empty is a
# config error.
$useSize = $false
$sizeInt = 0
if ($Content -eq '' -and $SizeBytes -ne '') {
    if (-not [int64]::TryParse($SizeBytes, [ref]$sizeInt)) {
        [Console]::Error.WriteLine("invalid -SizeBytes: '$SizeBytes' (expected integer)")
        exit 2
    }
    if ($sizeInt -lt 0) {
        [Console]::Error.WriteLine("-SizeBytes must be >= 0; got $sizeInt")
        exit 2
    }
    $useSize = $true
} elseif ($Content -eq '' -and $SizeBytes -eq '') {
    [Console]::Error.WriteLine("win-write: must provide either -Content or -SizeBytes")
    exit 2
}

# Strip a leading `/` or `\` so we can join with the drive letter root.
$relPath = $Path -replace '^[/\\]+', ''
if ($relPath -eq '') {
    [Console]::Error.WriteLine("win-write: -Path must name a file under the volume root; got '$Path'")
    exit 2
}

New-Item -ItemType Directory -Path $Diag -Force | Out-Null

$Vhdx = Get-VhdxPathFor -ImagePath $ImagePath

try {
    $state = Initialize-VhdxFromImg -ImagePath $ImagePath -Diag $Diag
    $letter = Mount-VhdxAndGetLetter -Vhdx $state.Vhdx

    $target = "${letter}:\$relPath"
    try {
        if ($useSize) {
            # Stream zero bytes in 64 KiB chunks so a -SizeBytes >2 GiB
            # works (.NET byte[] is Int32-indexed).
            $chunk = New-Object byte[] (64 * 1024)
            $fs = [System.IO.File]::Open($target, [System.IO.FileMode]::Create,
                [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
            try {
                $remaining = $sizeInt
                while ($remaining -gt 0) {
                    $n = [Math]::Min([int64]$chunk.Length, $remaining)
                    $fs.Write($chunk, 0, [int]$n)
                    $remaining -= $n
                }
                $fs.Flush($true)
            } finally { $fs.Close() }
            "wrote $sizeInt zero bytes to $target" | Out-File "$Diag\write-result.txt" -Encoding UTF8
        } else {
            # UTF-8, no BOM, no trailing newline — match v1's
            # `--content '<string>'` semantics from rust-ntfs write.
            $bytes = [System.Text.Encoding]::UTF8.GetBytes($Content)
            [System.IO.File]::WriteAllBytes($target, $bytes)
            "wrote $($bytes.Length) UTF-8 bytes to $target" | Out-File "$Diag\write-result.txt" -Encoding UTF8
        }
    } catch {
        $_.Exception.Message | Out-File "$Diag\write-error.txt" -Encoding UTF8
        throw
    }

    exit 0
} finally {
    Dismount-VhdxAndCleanup -Vhdx $Vhdx -ImagePath $ImagePath -KeepImage $KeepImageBool
}
