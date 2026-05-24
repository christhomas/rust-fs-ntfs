#!/usr/bin/env bash
# scripts/matrix-verify.sh — quickly check whether the working tree's
# binary is sealed by the committed test-diagnostics/matrix-results.json.
#
# Two-step check:
#   1. tested_at_sha == HEAD (cheap — git rev-parse)
#   2. binary_sha256 == sha256(target/release/rust-ntfs)  (rebuild if necessary)
#
# The binary check is the *primary* seal evidence. The SHA check is a
# convenience for tooling that doesn't want to build. Either passing is
# acceptable; both failing means the working tree is unsealed and a
# matrix re-run is needed before claiming verification.
#
# Usage:
#   bash scripts/matrix-verify.sh          # check only; exit 0 if sealed
#   bash scripts/matrix-verify.sh --build  # rebuild before checking
#   bash scripts/matrix-verify.sh --quiet  # suppress output
#
# Exit codes:
#   0 — sealed (either SHA or binary check passed)
#   1 — unsealed (both checks failed)
#   2 — missing or invalid baseline file

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

build=0
quiet=0
for arg in "$@"; do
    case "$arg" in
        --build) build=1 ;;
        --quiet) quiet=1 ;;
        *)       echo "unknown arg: $arg" >&2; exit 2 ;;
    esac
done

baseline="test-diagnostics/matrix-results.json"
if [ ! -f "$baseline" ]; then
    [ "$quiet" -eq 0 ] && echo "no baseline at $baseline" >&2
    exit 2
fi

head_sha=$(git rev-parse HEAD)
tested_at_sha=$(python3 -c "import json,sys; print(json.load(open('$baseline'))['tested_at_sha'])")

# SHA fast-path requires a clean working tree: tracked-file modifications
# would change the binary content even when HEAD matches tested_at_sha,
# making the SHA match a false positive. If dirty, fall through to the
# binary-hash check which always tells the truth.
if git diff-index --quiet HEAD -- && [ "$head_sha" = "$tested_at_sha" ]; then
    [ "$quiet" -eq 0 ] && echo "sealed by SHA ($head_sha)"
    exit 0
fi

# SHA check failed (probably rebased). Try binary hash.
binary_path="target/release/rust-ntfs"
if [ ! -f "$binary_path" ] || [ "$build" -eq 1 ]; then
    [ "$quiet" -eq 0 ] && echo "rebuilding binary for hash check (path-stable)..."
    # Same RUSTFLAGS as matrix-baseline.sh so a worktree's rebuild
    # matches the binary hash recorded by the seal run. Without this
    # remap, the absolute source path is embedded in panic strings
    # and the hashes mismatch even on identical source.
    export RUSTFLAGS="${RUSTFLAGS:-} \
        --remap-path-prefix=$PWD=. \
        --remap-path-prefix=$HOME/.cargo/registry=/registry"
    cargo build --release --quiet
fi
local_bin_sha=$(sha256sum "$binary_path" | awk '{print $1}')
tested_bin_sha=$(python3 -c "import json,sys; print(json.load(open('$baseline'))['binary_sha256'])")

if [ "$local_bin_sha" = "$tested_bin_sha" ]; then
    [ "$quiet" -eq 0 ] && echo "sealed by binary content (HEAD=$head_sha differs from tested_at_sha=$tested_at_sha, but binary is identical)"
    exit 0
fi

[ "$quiet" -eq 0 ] && cat >&2 <<EOF
UNSEALED:
  HEAD              = $head_sha
  tested_at_sha     = $tested_at_sha
  local binary SHA  = $local_bin_sha
  tested binary SHA = $tested_bin_sha

Run \`bash scripts/matrix-baseline.sh\` to re-test and refresh the JSON.
EOF
exit 1
