# Builds the two fixtures whose shapes only Windows can author:
#   * ntfs-attrlist.img: named $DATA streams overflow into extension records
#   * ntfs-compressed.img: classic NTFS/LZNT1-compressed $DATA
#
# Run from the repository root on an elevated Windows host. vhd_tool must be
# on PATH; CI installs the pinned rust-img-vhd release before invoking this.

$ErrorActionPreference = 'Stop'

. "$PSScriptRoot\..\scripts\fs-windows-test-harness\_lib.ps1"

$tempRoot = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [System.IO.Path]::GetTempPath() }

function New-WindowsFixture {
    param(
        [Parameter(Mandatory=$true)] [string]$ImagePath,
        [Parameter(Mandatory=$true)] [string]$Label,
        [Parameter(Mandatory=$true)] [scriptblock]$Populate
    )

    $image = [System.IO.Path]::GetFullPath($ImagePath)
    $diag = Join-Path $tempRoot ([System.IO.Path]::GetFileNameWithoutExtension($image))
    New-Item -ItemType Directory -Path $diag -Force | Out-Null
    Remove-Item $image,([System.IO.Path]::ChangeExtension($image, '.vhd')) `
        -Force -ErrorAction SilentlyContinue

    $stream = [System.IO.File]::Create($image)
    try { $stream.SetLength(256MB) } finally { $stream.Close() }

    $state = Initialize-VhdFromImg -ImagePath $image -Diag $diag
    try {
        $letter = Mount-VhdAndGetLetter -Vhd $state.Vhd
        Format-Volume -DriveLetter $letter -FileSystem NTFS `
            -AllocationUnitSize 4096 -NewFileSystemLabel $Label `
            -Force -Confirm:$false | Out-Null
        & $Populate "${letter}:\"
        Sync-VhdToImg -Vhd $state.Vhd -ImagePath $image
    } finally {
        Get-DiskImage -ImagePath $state.Vhd -ErrorAction SilentlyContinue |
            Where-Object Attached |
            Dismount-DiskImage -ErrorAction SilentlyContinue | Out-Null
        Remove-Item $state.Vhd -Force -ErrorAction SilentlyContinue
    }

    if (-not (Test-Path $image) -or (Get-Item $image).Length -ne 256MB) {
        throw "fixture was not written at the expected size: $image"
    }
}

New-WindowsFixture -ImagePath 'test-disks/ntfs-attrlist.img' -Label 'ATTRLIST' -Populate {
    param($root)
    $file = Join-Path $root 'many.bin'
    [System.IO.File]::WriteAllBytes($file, [byte[]](0x42))

    # Each resident named stream consumes an attribute header, UTF-16 name,
    # and value in the file record. Enough streams force Windows to create an
    # $ATTRIBUTE_LIST and extension records; the Rust tests assert that shape.
    for ($i = 0; $i -lt 96; $i++) {
        $name = 'stream-{0:D3}' -f $i
        $bytes = [System.Text.Encoding]::UTF8.GetBytes("payload-$name")
        [System.IO.File]::WriteAllBytes("${file}:$name", $bytes)
    }
}

New-WindowsFixture -ImagePath 'test-disks/ntfs-compressed.img' -Label 'COMPRESSED' -Populate {
    param($root)
    $file = Join-Path $root 'comp.txt'
    $bytes = New-Object byte[] 200000
    $pattern = [System.Text.Encoding]::ASCII.GetBytes('ABC')
    for ($i = 0; $i -lt $bytes.Length; $i++) { $bytes[$i] = $pattern[$i % 3] }
    [System.IO.File]::WriteAllBytes($file, $bytes)

    & compact.exe /C /I /Q $file | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "compact.exe failed with exit $LASTEXITCODE" }
    $attrs = (Get-Item $file -Force).Attributes
    if (($attrs -band [System.IO.FileAttributes]::Compressed) -eq 0) {
        throw 'Windows did not mark comp.txt as NTFS-compressed'
    }
}

Get-Item test-disks/ntfs-attrlist.img,test-disks/ntfs-compressed.img |
    Format-Table Name,Length
