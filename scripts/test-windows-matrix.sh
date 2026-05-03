#!/usr/bin/env bash
# test-windows-matrix.sh -- local scaffold around the matrix testing core.
#
# Architecture (mirrors GH Actions validate-mkfs-windows-matrix):
#
#   ┌── this script (Mac-side scaffold) ───────────────────┐
#   │  tar source → ssh → invoke testing core → pull diag  │
#   └────────────────────┬─────────────────────────────────┘
#                        │
#   ┌── testing core (runs inside Windows; identical in CI) ┐
#   │  cargo test --release --test matrix --                │
#   │      --test-threads=1 --no-fail-fast                  │
#   └───────────────────────────────────────────────────────┘
#
# In CI the same testing core runs on a windows-latest runner with no
# scaffold — `cargo test --release --test matrix` is the entire CI step.
# This script only exists to bridge Mac→Windows for local iteration.
#
# Output: per-scenario PASS/FAIL listing from libtest-mimic plus an
# aggregate "X passed; Y failed" line. The full diag tree (per-scenario
# manifest.json, ours-mft-16recs.bin / reference-mft-16recs.bin for
# byte-diff, chkdsk output, NTFS event log) lands at
# test-diagnostics/run-<timestamp>/ at the repo root — that's the
# evidence packet an automated fix-loop reads to decide its next
# iteration.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Auto-load resolved settings written by scripts/setup-local.sh so
# contributors don't have to set env vars on every invocation. Anything
# already in the environment overrides the file (so CI / one-off
# debugging can still inject overrides).
if [[ -f "${REPO_ROOT}/.test-env" ]]; then
    # shellcheck disable=SC1091
    source "${REPO_ROOT}/.test-env"
fi

if [[ -z "${VM_HOST:-}" ]]; then
    echo "VM_HOST is not set. Run scripts/setup-local.sh first, or export VM_HOST=user@host." >&2
    exit 2
fi
VM_WORKDIR="${VM_WORKDIR:-C:/Users/${VM_HOST%%@*}/dev/rust-fs-ntfs-matrix}"
SSH_OPTS="${SSH_OPTS:-}"
# Always-on SSH timeouts so a dead VM fails fast instead of hanging
# this script forever. ConnectTimeout aborts the initial handshake;
# ServerAliveInterval/CountMax tear down a hung session if the VM
# stops responding mid-run (matrix's 25-scenario sweep had a 1+ hour
# silent hang on a powered-off VM before this was added).
SSH_OPTS="${SSH_OPTS} -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=4"
VM_WORKDIR_PS="${VM_WORKDIR//\//\\}"

TIMESTAMP="$(date +%Y%m%d-%H%M%S)"
# test-diagnostics/ at repo root; every run lands in run-<timestamp>/.
# Gitignored. An automated fix-loop reads results.json from this dir
# to decide its next iteration; a human inspects the per-scenario
# subdirs directly. Override with $DIAG_DIR if you'd rather have it
# elsewhere (e.g. on a faster disk).
DIAG_BASE="${DIAG_DIR:-${REPO_ROOT}/test-diagnostics}"
DIAG_LOCAL="${DIAG_BASE}/run-${TIMESTAMP}"
mkdir -p "${DIAG_LOCAL}"

# Optional libtest-mimic argv passthrough — e.g. a single scenario:
#   bash scripts/test-windows-matrix.sh mac-format-tiny-32mib
# or "--list" to show what would run without running anything.
TEST_ARGS=("$@")

cd "${REPO_ROOT}"

echo "[push] tar-ssh source -> ${VM_HOST}:${VM_WORKDIR}"
ssh ${SSH_OPTS} "${VM_HOST}" "if (-not (Test-Path '${VM_WORKDIR_PS}')) { New-Item -ItemType Directory -Path '${VM_WORKDIR_PS}' -Force | Out-Null }"
tar --exclude='./target' --exclude='./.git' --exclude='./diag' \
    --exclude='./diag-local-*' --exclude='./test-disks' \
    --exclude='./tarpaulin-report.html' --exclude='*.swp' \
    --exclude='.DS_Store' --exclude='./privatekey' --exclude='./privatekey.*' \
    -cf - . | \
    ssh ${SSH_OPTS} "${VM_HOST}" "tar -xf - -C '${VM_WORKDIR}'"

# The testing core. One command, identical to CI. --test-threads=1
# because VHDX mounts share global drive-letter state on Windows.
# (libtest-mimic doesn't fail-fast by default; every scenario runs and
# is reported regardless of others' results — that's the whole point.)
#
# --verbose is also forwarded as MATRIX_VERBOSE=1 in the remote env, so
# the test binary engages the per-step tree output even if a future
# argv-handling change in libtest-mimic ate the flag at the test side.
EXTRA_ARGS=""
VERBOSE_ENV_PREFIX=""
for arg in "${TEST_ARGS[@]}"; do
    if [[ "$arg" == "--verbose" ]]; then
        VERBOSE_ENV_PREFIX="\$env:MATRIX_VERBOSE='1'; "
        echo "[run]  --verbose detected — engaging per-step tree on remote"
    fi
done
if [[ ${#TEST_ARGS[@]} -gt 0 ]]; then
    EXTRA_ARGS=$(printf ' %q' "${TEST_ARGS[@]}")
fi

REMOTE_CMD="Set-Location '${VM_WORKDIR_PS}'; \$env:RUSTUP_TOOLCHAIN='stable-aarch64-pc-windows-gnullvm'; \$env:PATH=\"\$env:USERPROFILE\\.cargo\\bin;C:\\Program Files\\Cloudbase Solutions\\QEMU\\bin;\$env:PATH\"; ${VERBOSE_ENV_PREFIX}cargo test --release --test matrix -- --test-threads=1${EXTRA_ARGS}"

echo "[run]  cargo test --release --test matrix on ${VM_HOST}"
echo "[run]  remote: cargo test --release --test matrix -- --test-threads=1${EXTRA_ARGS}"
echo
set +e
ssh ${SSH_OPTS} "${VM_HOST}" "${REMOTE_CMD}"
RUN_EXIT=$?
set -e

echo
echo "[pull] test-diagnostics -> ${DIAG_LOCAL}"
# --strip-components=2 strips the leading "diag/matrix/" so the local
# layout is test-diagnostics/run-<ts>/<scenario>/... rather than
# test-diagnostics/run-<ts>/diag/matrix/<scenario>/...
ssh ${SSH_OPTS} "${VM_HOST}" "Set-Location '${VM_WORKDIR_PS}'; if (Test-Path 'diag\\matrix') { tar -cf - diag/matrix } else { exit 0 }" | \
    tar -xf - -C "${DIAG_LOCAL}" --strip-components=2 2>/dev/null || \
    echo "[pull] no diag/matrix on remote (build may have failed before any scenario ran)"

echo
echo "═══════════════════════════════════════════════════════════════"
if [[ -f "${DIAG_LOCAL}/results.json" ]]; then
    # Compact summary: count by status. (Avoids requiring jq.)
    PASS=$(grep -c '"status": "passed"' "${DIAG_LOCAL}/results.json" || true)
    FAIL=$(grep -c '"status": "failed"' "${DIAG_LOCAL}/results.json" || true)
    ERR=$(grep -c '"status": "errored"' "${DIAG_LOCAL}/results.json" || true)
    echo "results: ${PASS} passed, ${FAIL} failed, ${ERR} errored"
fi
echo "diagnostics: ${DIAG_LOCAL}"
echo "test exit:   ${RUN_EXIT}  (0 = all passed/ignored; non-zero = at least one failed)"
echo "═══════════════════════════════════════════════════════════════"

exit ${RUN_EXIT}
