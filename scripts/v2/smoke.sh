#!/usr/bin/env bash
# scripts/v2/smoke.sh — pre-merge VM smoke for PowerShell-touching PRs.
#
# The v2 cross-host dispatcher path runs through `scripts/v2/win-chkdsk.ps1`
# on the Windows VM. PowerShell + raw-disk I/O has surprises that don't
# show up in `cargo test` or pre-commit (clippy, fmt) — the only way to
# catch them early is an actual VM round-trip.
#
# This script runs the smallest cross-host scenario (`mac-format-tiny-32mib`,
# ~5 min wall clock) against the harness binary so any PR that touches
# `scripts/v2/*.ps1` or the harness substitution path can verify the
# dispatcher still works end-to-end before review.
#
# Use:
#     bash scripts/v2/smoke.sh            # single scenario, ~5 min
#     bash scripts/v2/smoke.sh --build    # rebuild rust-ntfs + harness first
#     bash scripts/v2/smoke.sh --ship     # tar source to VM (one-time after harness changes)
#
# Pass criterion: the named scenario produces VERDICT=passed against the
# v2 dispatcher. Same contract as `tests/matrix.rs` baseline.
#
# Cost note: on first invocation in a fresh checkout, --build + --ship
# add ~2 min for cargo + ~1 min for tar. Subsequent runs cache both.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${REPO_ROOT}"

if [[ -f "${REPO_ROOT}/.test-env" ]]; then
    # shellcheck disable=SC1091
    source "${REPO_ROOT}/.test-env"
fi

SCENARIO="${SMOKE_SCENARIO:-mac-format-tiny-32mib}"
DO_BUILD=0
DO_SHIP=0
for arg in "$@"; do
    case "$arg" in
        --build) DO_BUILD=1 ;;
        --ship)  DO_SHIP=1 ;;
        --help|-h)
            sed -n '2,/^$/p' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        *)
            echo "[smoke] unknown argument: ${arg}" >&2
            echo "[smoke] use --help for usage" >&2
            exit 2
            ;;
    esac
done

if [[ "${DO_BUILD}" == "1" ]]; then
    echo "[smoke] building rust-ntfs"
    cargo build --release --bin rust-ntfs
    echo "[smoke] building harness runner"
    cargo build --manifest-path vendor/harness/runner/Cargo.toml --release --bin run-matrix
fi

if [[ "${DO_SHIP}" == "1" ]]; then
    if [[ -z "${VM_HOST:-}" ]]; then
        echo "[smoke] VM_HOST unset; can't ship. Run scripts/setup-local.sh or set in .test-env." >&2
        exit 2
    fi
    if [[ -z "${VM_WORKDIR:-}" ]]; then
        echo "[smoke] VM_WORKDIR unset; can't ship. Run scripts/setup-local.sh or set in .test-env." >&2
        exit 2
    fi
    echo "[smoke] tar source -> ${VM_HOST}:${VM_WORKDIR}"
    VM_WORKDIR_PS="${VM_WORKDIR//\//\\}"
    ssh ${SSH_OPTS:-} "${VM_HOST}" "if (-not (Test-Path '${VM_WORKDIR_PS}')) { New-Item -ItemType Directory -Path '${VM_WORKDIR_PS}' -Force | Out-Null }"
    tar --exclude='./target' --exclude='./.git' --exclude='./diag' \
        --exclude='./test-disks' --exclude='./test-diagnostics' \
        --exclude='./vendor/harness/runner/target' \
        --exclude='*.swp' --exclude='.DS_Store' \
        --exclude='./privatekey' --exclude='./privatekey.*' \
        -cf - . | ssh ${SSH_OPTS:-} "${VM_HOST}" "tar -xf - -C '${VM_WORKDIR}'"
fi

echo "[smoke] running scenario: ${SCENARIO}"
rm -rf "test-diagnostics/matrix/${SCENARIO}" "nfs-${SCENARIO}.img" 2>/dev/null

START=$(date +%s)
set +e
HARNESS_CONSUMER_ROOT="${REPO_ROOT}" HARNESS_IMAGE_DIR="${REPO_ROOT}" \
    cargo run --manifest-path vendor/harness/runner/Cargo.toml \
              --release --bin run-matrix -- "${SCENARIO}"
RC=$?
set -e
ELAPSED=$(( $(date +%s) - START ))

echo
echo "═══════════════════════════════════════"
echo "  smoke verdict: $([[ $RC -eq 0 ]] && echo PASSED || echo "FAILED (rc=$RC)")"
echo "  elapsed:       ${ELAPSED}s"
echo "  scenario:      ${SCENARIO}"
echo "  diag:          test-diagnostics/matrix/${SCENARIO}/"
echo "═══════════════════════════════════════"

# Local cleanup so a re-run starts fresh.
rm -f "nfs-${SCENARIO}.img" 2>/dev/null

exit "${RC}"
