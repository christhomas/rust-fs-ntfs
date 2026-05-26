#!/usr/bin/env bash
# scripts/run-matrix.sh — wrapper around the fs-test-harness matrix
# runner that cleans up disk images on exit (success, failure, Ctrl-C,
# or any signal).
#
# Why a wrapper instead of fixing the harness directly:
# * The harness lives in `vendor/fs-test-harness/` (git submodule);
#   we don't own its lifecycle. Cleanup belongs in consumer code.
# * Each scenario in `test-matrix.json` points at
#   `diskimages/nfs-<scenario>.img`; `init-image` populates them and
#   has no opinion about who removes them. Without this trap the
#   directory accumulates ~3-5 GiB of stale images across runs.
#
# Multi-instance safety:
# * Each invocation creates its own /tmp backing dir ($$-suffixed) so
#   the EXIT cleanup cannot destroy a sibling instance's images.
# * The diskimages/ symlink is created only when absent — never
#   overwritten — so a persistent or manually-set symlink is left alone.
# * A mkdir-based lock keyed on the scenario filter prevents two
#   instances from running the same scenario set simultaneously.
#   Stale locks (from killed processes) can be removed with:
#     rm -rf /tmp/ntfs-matrix-lock-*
#
# Usage: same as `vendor/fs-test-harness/scripts/run-tests.sh`. All
# arguments pass straight through. Examples:
#
#   bash scripts/run-matrix.sh                  # full matrix
#   bash scripts/run-matrix.sh basic-ro-list    # substring filter
#   bash scripts/run-matrix.sh --list           # don't run, just list
#   bash scripts/run-matrix.sh --keep-images    # skip the cleanup trap
#                                                 (handy for byte-diff
#                                                  inspection)
#
# Exit codes: pass-through from the harness runner. The trap fires
# regardless of exit code.

set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root" || {
    echo "[run-matrix] fatal: cannot cd to repo_root=$repo_root" >&2
    exit 1
}

# Allow `--keep-images` to opt out of cleanup. Strip it before
# forwarding so the harness runner doesn't see an unknown flag.
keep_images=0
forwarded_args=()
for arg in "$@"; do
    case "$arg" in
        --keep-images) keep_images=1 ;;
        *) forwarded_args+=("$arg") ;;
    esac
done

# ── Per-instance disk-image directory ──────────────────────────────────────
# Each run-matrix.sh instance gets its own /tmp backing dir ($$-suffixed).
# EXIT cleanup removes that dir directly so it can't affect a sibling
# instance's images. The diskimages/ symlink is created only when absent;
# a pre-existing symlink (persistent or set by another instance) is left
# as-is.
diskimages_tmp="/tmp/ntfs-matrix-diskimages-$$"
mkdir -p "$diskimages_tmp"
if [[ ! -L "$repo_root/diskimages" && ! -e "$repo_root/diskimages" ]]; then
    ln -s "$diskimages_tmp" "$repo_root/diskimages"
fi

# ── Per-scenario-filter lock ────────────────────────────────────────────────
# Prevent two invocations with the same scenario filter from racing (they
# would write to the same .img file). Uses mkdir atomicity. The lock key
# is the normalised filter string (or "full" for no filter).
lock_key="${forwarded_args[*]:-full}"
lock_key="${lock_key//[^a-zA-Z0-9_-]/_}"
scenario_lock="/tmp/ntfs-matrix-lock-${lock_key}"
if ! mkdir "$scenario_lock" 2>/dev/null; then
    existing_pid=$(cat "$scenario_lock/pid" 2>/dev/null || echo "?")
    echo "[run-matrix] scenario filter '${lock_key}' is already running (pid ${existing_pid})" >&2
    echo "[run-matrix] if stale: rm -rf ${scenario_lock}" >&2
    exit 1
fi
echo "$$" > "$scenario_lock/pid"

cleanup() {
    if [ "$keep_images" -eq 1 ]; then
        echo "[run-matrix] --keep-images set; leaving $diskimages_tmp in place" >&2
    else
        local count
        count=$(find "$diskimages_tmp" -maxdepth 1 -name '*.img' -type f 2>/dev/null | wc -l | tr -d ' ')
        if [ "$count" -gt 0 ]; then
            echo "[run-matrix] cleanup: removing $count image(s) from $diskimages_tmp" >&2
            find "$diskimages_tmp" -maxdepth 1 -name '*.img' -type f -delete
        fi
    fi
    rm -rf "$scenario_lock"
}

# Single EXIT trap does all the work. The signal traps re-raise as
# `exit <128 + signum>` (the conventional code for that signal), which
# flows into the EXIT trap once. Trapping cleanup directly on signals
# AND on EXIT would double-fire it (PR #48 review, greptile-apps[bot]).
trap cleanup EXIT
trap 'exit 130' INT   # 128 + SIGINT  (2)
trap 'exit 143' TERM  # 128 + SIGTERM (15)
trap 'exit 129' HUP   # 128 + SIGHUP  (1)
trap 'exit 131' QUIT  # 128 + SIGQUIT (3)

# Forward to the real runner.
bash "$repo_root/vendor/fs-test-harness/scripts/run-tests.sh" "${forwarded_args[@]+"${forwarded_args[@]}"}"
