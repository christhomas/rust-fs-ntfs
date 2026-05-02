#!/usr/bin/env bash
# test-windows-local.sh — orchestrate a local Windows-VM iteration.
#
# What it does:
#   1. Tar the rust-fs-ntfs source dir (excluding target/, .git/, diag/)
#      and stream it via SSH onto the Windows VM, untarring into the
#      remote workdir.
#   2. SSH in and run scripts/run-windows-test.ps1 — builds mkfs_ntfs.exe,
#      formats nfs.img, wraps in a GPT-partitioned VHDX, mounts, runs
#      chkdsk + a Microsoft format.com reference, dumps diag/.
#   3. Stream diag/ back to ./diag-local-<timestamp>/ on the Mac.
#   4. Print a summary: chkdsk verdict + path to diag dir.
#
# Designed for parity with the GitHub Actions validate-mkfs-windows job
# (`.github/workflows/ci.yml`) but at ~10–30s per iteration vs ~2 min
# for a CI cycle. Suitable for the corroborated-debug iteration loop.
#
# Requires: SSH access to the VM (no password — keys), Rust + qemu-img
# already installed on the VM (one-time setup, see docs).

set -euo pipefail

VM_HOST="${VM_HOST:-chris@192.168.213.145}"
VM_WORKDIR="${VM_WORKDIR:-C:/Users/chris/dev/rust-fs-ntfs}"
# PowerShell-safe path form for the workdir.
VM_WORKDIR_PS="${VM_WORKDIR//\//\\}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TIMESTAMP="$(date +%Y%m%d-%H%M%S)"
# Diag dirs go into a tmp area outside the repo so they never leak into
# git history (some are big, all are throwaway). Override with $DIAG_DIR
# if you want them somewhere persistent.
DIAG_BASE="${DIAG_DIR:-${TMPDIR:-/tmp}/rust-fs-ntfs-diag}"
DIAG_LOCAL="${DIAG_BASE}/iter-${TIMESTAMP}"
mkdir -p "${DIAG_BASE}"

cd "${REPO_ROOT}"

# ─── 1. Push source ──────────────────────────────────────────────────
# Tar streams cleanly over ssh; --exclude removes the heavy stuff. The
# remote untar overwrites whatever was there, so each run starts from
# a known tree. (No need for rsync incrementality — source is ~10 MB.)
echo "[push] tar-ssh source -> ${VM_HOST}:${VM_WORKDIR}"
ssh "${VM_HOST}" "if (-not (Test-Path '${VM_WORKDIR_PS}')) { New-Item -ItemType Directory -Path '${VM_WORKDIR_PS}' -Force | Out-Null }"
tar --exclude='./target' --exclude='./.git' --exclude='./diag' --exclude='./diag-local-*' \
    --exclude='./test-disks' --exclude='./tarpaulin-report.html' \
    --exclude='*.swp' --exclude='.DS_Store' \
    -cf - . | \
    ssh "${VM_HOST}" "tar -xf - -C '${VM_WORKDIR}'"

# ─── 2. Run test on VM ───────────────────────────────────────────────
# Optional per-scenario overrides from the environment. The runner has
# matching defaults (256 MiB volume / 384 MiB wrapper / 4 KiB cluster /
# label CITEST).
PS_ARGS=""
[[ -n "${VOL_MB:-}" ]]       && PS_ARGS+=" -VolumeSizeMb ${VOL_MB}"
[[ -n "${WRAP_MB:-}" ]]      && PS_ARGS+=" -WrapperSizeMb ${WRAP_MB}"
[[ -n "${CLUSTER_SIZE:-}" ]] && PS_ARGS+=" -ClusterSize ${CLUSTER_SIZE}"
if [[ -n "${LABEL+x}" ]]; then
    # PowerShell takes the next token as the value; quote so spaces
    # survive (and still pass through the ssh shell).
    PS_ARGS+=" -Label '${LABEL//\'/\'\'}'"
fi
echo "[run]  scripts/run-windows-test.ps1${PS_ARGS} on ${VM_HOST}"
set +e
ssh "${VM_HOST}" "Set-Location '${VM_WORKDIR_PS}'; powershell -ExecutionPolicy Bypass -File '.\\scripts\\run-windows-test.ps1'${PS_ARGS}"
RUN_EXIT=$?
set -e

# ─── 3. Pull diag/ back ──────────────────────────────────────────────
echo "[pull] diag/ -> ${DIAG_LOCAL}"
mkdir -p "${DIAG_LOCAL}"
# Stream the diag/ tree back via tar over ssh (same trick).
ssh "${VM_HOST}" "Set-Location '${VM_WORKDIR_PS}'; tar -cf - diag" | \
    tar -xf - -C "${DIAG_LOCAL}" --strip-components=1 2>/dev/null || \
    echo "[pull] no diag/ on remote (test may have failed before any output)"

# ─── 4. Summarise ────────────────────────────────────────────────────
echo
echo "═══════════════════════════════════════════════════════════════"
if [[ -f "${DIAG_LOCAL}/chkdsk-readonly.txt" ]]; then
    echo "chkdsk readonly output:"
    sed 's/^/  /' "${DIAG_LOCAL}/chkdsk-readonly.txt"
fi
if [[ -f "${DIAG_LOCAL}/chkdsk-readonly-exit.txt" ]]; then
    echo
    cat "${DIAG_LOCAL}/chkdsk-readonly-exit.txt"
fi
if [[ -f "${DIAG_LOCAL}/chkdsk-scan-exit.txt" ]]; then
    cat "${DIAG_LOCAL}/chkdsk-scan-exit.txt"
fi
echo
echo "diag dir: ${DIAG_LOCAL}"
echo "remote run exit: ${RUN_EXIT}"
echo "═══════════════════════════════════════════════════════════════"

exit ${RUN_EXIT}
