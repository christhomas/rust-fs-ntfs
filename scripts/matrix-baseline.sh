#!/usr/bin/env bash
# scripts/matrix-baseline.sh — run the full 42-scenario matrix and write
# the resulting test-diagnostics/matrix-results.json baseline file.
#
# The baseline file binds together:
#   * tested_at_sha          — git HEAD at run time
#   * binary_sha256          — sha256 of target/release/rust-ntfs
#   * VM metadata            — Windows + ntfs.sys + chkdsk versions
#   * harness_submodule_sha  — exact vendored harness commit
#   * test_matrix_json_sha256— hash of the scenarios definition
#   * per-scenario status, verdict_shape, chkdsk exit codes
#
# The result file is the *primary* evidence for a sealed commit:
# `binary_sha256 == sha256(target/release/rust-ntfs)` is the load-bearing
# check (content-addressable; survives rebase / squash-merge).
#
# Usage:
#   bash scripts/matrix-baseline.sh           # full matrix, ~3-4 hours
#   bash scripts/matrix-baseline.sh --smoke   # 5 representative scenarios, ~15 min
#
# All output (stdout + stderr) is always tee'd to:
#   /tmp/test-harness-full-matrix.log
# Tail that file in a second terminal to follow progress:
#   tail -f /tmp/test-harness-full-matrix.log
#
# Reads .test-env for VM_HOST / SSH_KEY. Writes:
#   * test-diagnostics/matrix-results.json
# Exit 0 iff every scenario passed AND the JSON was written.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

# Tee all output to a fixed path so `tail -f /tmp/test-harness-full-matrix.log`
# always works regardless of how the script is invoked.
exec > >(tee /tmp/test-harness-full-matrix.log) 2>&1

mode="full"
case "${1:-}" in
    --smoke) mode="smoke"; shift ;;
    --full)  mode="full"; shift ;;
    "") ;;
    *) echo "[matrix-baseline] unknown arg: $1 (expected --full or --smoke)" >&2; exit 2 ;;
esac

if [ ! -f .test-env ]; then
    echo "[matrix-baseline] fatal: .test-env not found (need VM_HOST / SSH_KEY)" >&2
    exit 1
fi
# shellcheck disable=SC1091
source .test-env

# 1. Build the binary with path-stable rustc flags so binary_sha256 is
#    invariant across worktrees / machines. Without these remaps, rustc
#    embeds the absolute source path in panic strings + debug info, so
#    `cargo build --release` of the same source produces different
#    binaries from different paths (e.g. /Volumes/.../rust-fs-ntfs vs
#    /Volumes/.../rust-fs-ntfs-s4). That breaks the seal-by-binary-hash
#    property documented in .claude/skills/wtx/SKILL.md.
echo "[matrix-baseline] cargo build --release (path-stable)"
export RUSTFLAGS="${RUSTFLAGS:-} \
    --remap-path-prefix=$PWD=. \
    --remap-path-prefix=$HOME/.cargo/registry=/registry"
cargo build --release --quiet

# 2. Run the matrix.
matrix_log="$(mktemp -t matrix-baseline.XXXXXX.log)"
trap 'rm -f "$matrix_log"' EXIT
# Note: matrix_log is a separate internal temp file used only by
# _matrix-collect-vm.sh for metadata extraction. The user-facing
# output always goes to /tmp/test-harness-full-matrix.log (above).

run_exit=0
if [ "$mode" = "smoke" ]; then
    # Smoke set: 5 scenarios covering small + large volume, write op,
    # mkdir+chkdsk, repeat mounts, delete path.
    smoke_scenarios=(
        mac-format-basic-256mib
        mac-format-tiny-32mib
        mac-format-mkdir-set-dirty-win-chkdsk
        mac-format-mac-write-win-repeat-mount-3-win-chkdsk
        mac-format-win-write-many-win-delete-half-win-chkdsk
    )
    echo "[matrix-baseline] smoke: ${smoke_scenarios[*]}"
    smoke_failed=0
    for s in "${smoke_scenarios[@]}"; do
        bash scripts/run-matrix.sh "$s" 2>&1 | tee -a "$matrix_log"
        s_exit=${PIPESTATUS[0]}
        [ "$s_exit" -ne 0 ] && run_exit="$s_exit"
    done
else
    echo "[matrix-baseline] full matrix (~3-4 hours)"
    bash scripts/run-matrix.sh 2>&1 | tee "$matrix_log"
    run_exit=${PIPESTATUS[0]}
fi

# 3. Collect VM metadata + per-scenario verdicts (runs regardless of pass/fail).
echo "[matrix-baseline] collecting VM metadata + verdicts"
bash scripts/_matrix-collect-vm.sh "$matrix_log"

echo "[matrix-baseline] wrote test-diagnostics/matrix-results.json"
echo "[matrix-baseline] tested_at_sha=$(git rev-parse HEAD)"
echo "[matrix-baseline] binary_sha256=$(sha256sum target/release/rust-ntfs | awk '{print $1}')"

exit "$run_exit"
