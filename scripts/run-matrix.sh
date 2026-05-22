#!/usr/bin/env bash
# scripts/run-matrix.sh — wrapper around the fs-test-harness matrix
# runner that cleans up `diskimages/` on exit (success, failure,
# Ctrl-C, or any signal).
#
# Why a wrapper instead of fixing the harness directly:
# * The harness lives in `vendor/fs-test-harness/` (git submodule);
#   we don't own its lifecycle. Cleanup belongs in consumer code.
# * Each scenario in `test-matrix.json` points at
#   `diskimages/nfs-<scenario>.img`; `init-image` populates them and
#   has no opinion about who removes them. Without this trap the
#   directory accumulates ~3-5 GiB of stale images across runs.
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
cd "$repo_root"

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

cleanup_diskimages() {
    local rc=$?
    if [ "$keep_images" -eq 1 ]; then
        echo "[run-matrix] --keep-images set; leaving diskimages/ in place" >&2
        return
    fi
    if [ -d "$repo_root/diskimages" ]; then
        local count
        count=$(find "$repo_root/diskimages" -maxdepth 1 -name '*.img' -type f 2>/dev/null | wc -l | tr -d ' ')
        if [ "$count" -gt 0 ]; then
            echo "[run-matrix] cleanup: removing $count image(s) from diskimages/" >&2
            find "$repo_root/diskimages" -maxdepth 1 -name '*.img' -type f -delete
        fi
    fi
    # Re-emit the harness's exit code so a `set -e` caller still sees it.
    exit "$rc"
}

# Trap on every common termination path. EXIT covers normal-exit and
# error-exit; INT/TERM/HUP/QUIT cover signals that wouldn't otherwise
# trigger EXIT cleanly (e.g. SIGINT during a long step).
trap cleanup_diskimages EXIT INT TERM HUP QUIT

# Forward to the real runner.
bash "$repo_root/vendor/fs-test-harness/scripts/run-tests.sh" "${forwarded_args[@]+"${forwarded_args[@]}"}"
