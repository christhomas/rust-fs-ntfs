#!/usr/bin/env bash
# test-verdict.sh TIER [TIER...]  -- the last line a green `chore test` prints
#
# One number and one path. Each named tier has a log in tmp/logs/<tier>.log
# (written by scripts/tier.sh); this reads the cargo result lines out of them
# and prints the total.
#
# WHAT THE NUMBER COUNTS is EXECUTIONS, not distinct tests: `chore test` runs
# the unit tests three times (release, debug, and inside the whole suite) and
# the mkfs files twice, on purpose. A distinct-test count would be the same
# number whether or not a tier still ran, and the thing worth noticing here
# is a tier quietly stopping -- which shows up as a drop in this number and
# in nothing else. The per-run floors that FAIL on that drop are in ci.yml.
#
# A missing log is a failure, not a zero: it means a tier did not run.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOGS="$REPO/tmp/logs"

[ $# -gt 0 ] || { echo "test-verdict.sh: usage: test-verdict.sh TIER [TIER...]" >&2; exit 2; }

total=0
tiers=0
for tier in "$@"; do
    log="$LOGS/$tier.log"
    if [ ! -f "$log" ]; then
        echo "test-verdict.sh: $log is missing -- the $tier tier did not run." >&2
        exit 1
    fi
    # `test result: ok. 37 passed; 0 failed; ...`, one per test binary.
    n="$(awk '/^test result: ok\./ { sum += $4 } END { print sum + 0 }' "$log")"
    total=$(( total + n ))
    tiers=$(( tiers + 1 ))
done

printf '%s\n' "test: $tiers tiers green, $total test executions -- logs in tmp/logs/"
