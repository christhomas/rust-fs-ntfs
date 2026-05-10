#!/usr/bin/env bash
# setup-windows-vm.sh -- one-time provisioning of a Windows ARM64 VM
# for local test iteration. Mac-side wrapper around setup-windows-vm.ps1.
#
# What it does:
#   1. SSH to the VM and copy setup-windows-vm.ps1 over.
#   2. Execute it (idempotent -- safe to re-run).
#   3. Verify the test pipeline can build at least once.
#
# After this completes, the matrix runner (via fs-test-harness's
# scripts/test-windows-matrix.sh) should work against the VM.
#
# Cost (one-time, on a fresh VM):
#   * Rustup     ~50 MB  / ~30 s
#   * gnullvm    ~600 MB / ~2 min  (Rust toolchain + std)
#   * LLVM-MinGW ~250 MB / ~30 s
#   * vhd_tool   ~built locally from rust-img-vhd source / <1 min
#   Total: ~900 MB / ~4 min over a typical home connection.

set -euo pipefail

VM_HOST="${VM_HOST:-chris@192.168.213.145}"
VM_WORKDIR="${VM_WORKDIR:-C:/Users/chris/dev/rust-fs-ntfs}"
VM_WORKDIR_PS="${VM_WORKDIR//\//\\}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

echo "[setup] Pushing setup-windows-vm.ps1 to ${VM_HOST}"
ssh "${VM_HOST}" "if (-not (Test-Path '${VM_WORKDIR_PS}\\scripts')) { New-Item -ItemType Directory -Path '${VM_WORKDIR_PS}\\scripts' -Force | Out-Null }"
scp "${REPO_ROOT}/scripts/setup-windows-vm.ps1" "${VM_HOST}:${VM_WORKDIR}/scripts/setup-windows-vm.ps1"

echo "[setup] Running setup-windows-vm.ps1 on ${VM_HOST}"
ssh "${VM_HOST}" "powershell -ExecutionPolicy Bypass -File '${VM_WORKDIR_PS}\\scripts\\setup-windows-vm.ps1'"

echo
echo "[setup] Done. Try a real iteration:"
echo "    bash ${REPO_ROOT}/scripts/test-windows-local.sh"
