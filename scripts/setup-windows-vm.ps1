# setup-windows-vm.ps1 -- one-time provisioning of a Windows ARM64 VM
# for running scripts/run-windows-test.ps1 (the local mirror of the
# validate-mkfs-windows CI job).
#
# What this installs:
#   1. Rustlang.Rustup -- rustup itself.
#   2. stable-aarch64-pc-windows-gnullvm toolchain -- Rust without the
#      need for Microsoft Visual C++ Build Tools (saves ~3 GB vs MSVC
#      and avoids the VS install dance entirely).
#   3. MartinStorsjo.LLVM-MinGW.UCRT -- bundles
#      `aarch64-w64-mingw32-clang.exe`, the linker the gnullvm target
#      requires for build scripts that compile native code (proc-macro2,
#      quote, etc.). ~200 MB self-contained.
#   4. vhd_tool from antimatter-studios/rust-img-vhd -- creates the
#      VHD wrapper used by the harness (Mount-DiskImage refuses raw
#      images, only VHD/VHDX/ISO; vhd_tool builds the wrapper).
#
# Why these specific components:
#   - We picked gnullvm over MSVC because MSVC pulls in 3+ GB of Visual
#     Studio Build Tools and requires accepting Microsoft's commercial
#     license. gnullvm uses LLVM tooling end-to-end which has a clean
#     license and ~10x smaller footprint.
#   - LLVM-MinGW (Martin Storsjo's distribution) is the canonical way
#     to get aarch64-w64-mingw32-clang on Windows ARM64. Rustup's
#     gnullvm target ships rust-lld but expects this clang for build
#     scripts.
#   - vhd_tool creates the GPT-partitioned wrapper that lets
#     Mount-DiskImage attach our raw .img -- Windows Mount-DiskImage
#     refuses raw images, only VHD/VHDX/ISO. See
#     `docs/chkdsk-findings.md` iter1-2 for why this wrapper is needed.
#
# Usage:
#   - Run on the VM directly (admin or non-admin both fine for winget):
#       powershell -ExecutionPolicy Bypass -File setup-windows-vm.ps1
#   - Run from the Mac via the orchestrator's setup mode:
#       bash scripts/setup-windows-vm.sh
#
# Idempotent: every step checks before installing, so re-running is safe.

param(
    [string]$Workdir = "$env:USERPROFILE\dev"
)

$ErrorActionPreference = "Continue"  # winget writes progress to stderr;
                                     # don't abort on those.

function Test-WingetPackage {
    param([string]$Id)
    # winget list returns 0 if package is installed, non-zero otherwise.
    # The output also lists packages that *match* the ID -- we need an
    # exact match, so grep for the precise Id in column 2.
    $listing = winget list --id $Id --exact 2>&1 | Out-String
    return $listing -match [regex]::Escape($Id)
}

function Install-IfMissing {
    param([string]$Id, [string]$Description)
    Write-Host "[setup] $Description ($Id)"
    if (Test-WingetPackage -Id $Id) {
        Write-Host "        already installed -- skipping"
        return
    }
    winget install $Id --accept-source-agreements --accept-package-agreements --silent 2>&1 |
        Select-Object -Last 3 | ForEach-Object { Write-Host "        $_" }
}

# ---------- 1-2. Rust + gnullvm toolchain --------------------------------
Install-IfMissing -Id "Rustlang.Rustup" -Description "Rustup (Rust installer)"

$cargoBin = "$env:USERPROFILE\.cargo\bin"
$env:PATH = "$cargoBin;$env:PATH"

if (-not (Get-Command rustup -ErrorAction SilentlyContinue)) {
    throw "rustup not on PATH after install -- shell restart may be needed"
}

Write-Host "[setup] rustup default toolchain = stable-aarch64-pc-windows-gnullvm"
$current = (rustup show active-toolchain 2>&1) -replace '\s.*',''
if ($current -ne "stable-aarch64-pc-windows-gnullvm") {
    rustup default stable-aarch64-pc-windows-gnullvm 2>&1 |
        Select-Object -Last 3 | ForEach-Object { Write-Host "        $_" }
} else {
    Write-Host "        already set"
}

# ---------- 3. LLVM-MinGW (aarch64-w64-mingw32-clang) --------------------
Install-IfMissing -Id "MartinStorsjo.LLVM-MinGW.UCRT" `
    -Description "LLVM-MinGW (linker for the gnullvm target)"

# ---------- 4. Workdir ---------------------------------------------------
$workdirPath = $Workdir.TrimEnd('\','/')
if (-not (Test-Path $workdirPath)) {
    New-Item -ItemType Directory -Path $workdirPath -Force | Out-Null
}
Write-Host "[setup] workdir: $workdirPath"

# ---------- 5. vhd_tool from rust-img-vhd (wrapper writer) ---------------
# Clones antimatter-studios/rust-img-vhd into the workdir, builds + installs
# `vhd_tool` to ~/.cargo/bin (which is on PATH after rustup setup). The
# harness's _lib.ps1::Initialize-VhdFromImg invokes `vhd_tool create-fixed`
# directly. See docs/phase-1e-design.md for the design rationale (Path B:
# build on the VM rather than cross-compile from the Mac).
$vhdRepoDir = Join-Path $workdirPath "rust-img-vhd"
if (-not (Test-Path $vhdRepoDir)) {
    Write-Host "[setup] cloning rust-img-vhd into $vhdRepoDir"
    git clone --depth 1 https://github.com/antimatter-studios/rust-img-vhd.git $vhdRepoDir 2>&1 |
        Select-Object -Last 3 | ForEach-Object { Write-Host "        $_" }
} else {
    Write-Host "[setup] rust-img-vhd already cloned; pulling latest"
    Push-Location $vhdRepoDir
    git pull --ff-only 2>&1 | Select-Object -Last 3 | ForEach-Object { Write-Host "        $_" }
    Pop-Location
}

Write-Host "[setup] cargo install --path $vhdRepoDir --bin vhd_tool --locked"
Push-Location $vhdRepoDir
cargo install --path . --bin vhd_tool --locked 2>&1 |
    Select-Object -Last 3 | ForEach-Object { Write-Host "        $_" }
Pop-Location

# ---------- 6. Verify everything ----------------------------------------
$env:PATH = "$cargoBin;$env:PATH"

Write-Host ""
Write-Host "=== Verification ==="
& rustc --version 2>&1 | Select-Object -First 1
& cargo --version 2>&1 | Select-Object -First 1
$vhdToolBin = Get-Command vhd_tool -ErrorAction SilentlyContinue
if ($vhdToolBin) {
    Write-Host "vhd_tool: $($vhdToolBin.Source)"
} else {
    Write-Host "WARN: vhd_tool not on PATH; new shell may be needed"
}
$mingwClang = Get-Command "aarch64-w64-mingw32-clang*" -ErrorAction SilentlyContinue |
    Select-Object -First 1
if ($mingwClang) {
    Write-Host "aarch64-w64-mingw32-clang: $($mingwClang.Source)"
} else {
    Write-Host "WARN: aarch64-w64-mingw32-clang not on PATH; new shell may be needed"
}

Write-Host ""
Write-Host "=== Setup complete ==="
Write-Host "Run scripts/run-windows-test.ps1 from the workdir to test."
