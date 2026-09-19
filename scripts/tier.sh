#!/usr/bin/env bash
# tier.sh TIER -- COMMAND [ARG...]
#
# One test tier, run QUIETLY and under a budget. The whole run goes to
# tmp/logs/<TIER>.log; a pass prints one verdict line naming the log, a
# failure prints the tail of it, and a run that passed but printed more than
# its budget fails with status 65 -- told apart from a failing suite, which
# exits with the suite's own status.
#
# The work is done by the harness's scripts/output-budget.sh (the pinned
# ../fs-windows-test-harness sibling, v4.1.0 and later). This file only owns
# the part that is ours: which tiers exist, and how much each may print.
#
# WHY THE BUDGET IS PART OF THE TIER and not a CI-only check: the reader who
# pays most for a noisy suite is the one running it -- a person scrolling, or
# an agent that re-reads its whole transcript on every step and so pays for
# one loud run many times over. A rule only CI enforces is a rule the tree
# drifts away from between pull requests. `chore <tier>` and ci.yml both run
# through here, so they are held to the same number.
#
# THE BUDGETS ARE ONE TABLE, BELOW, and every number in it was measured. A
# tier with no row is refused rather than run unbudgeted. Raise one
# deliberately when a tier grows, and say where the new number came from; a
# budget nobody can breach measures nothing.
#
# VERBOSE. `FWTH_VERBOSE=1`, or `--verbose`/`-v` in a chore invocation's
# CLI_ARGS (`chore test:unit -- --verbose`), streams the run as it happens as
# well as logging it. It does NOT lift the budget: the log is the same size
# either way.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUDGET="$REPO/../fs-windows-test-harness/scripts/output-budget.sh"

[ $# -ge 3 ] || { echo "tier.sh: usage: tier.sh TIER -- COMMAND [ARG...]" >&2; exit 2; }
TIER="$1"; shift
[ "$1" = "--" ] || { echo "tier.sh: expected -- after the tier name, got '$1'" >&2; exit 2; }
shift

# Lines / bytes each tier may print on a PASSING run. Measured 2026-09-18:
# locally on an arm64 Mac, cold (every crate compiled -- a line per crate)
# and warm; and on GitHub's runners, from the job logs of run 35294303960
# (main at 6670e25) with the timestamps stripped. Each budget is the larger
# measurement plus roughly a third, so ordinary growth fits and a change in
# kind -- a test that starts printing, a flag that turns on per-test output
# -- does not. The suite is measured on CI only: without the fixtures (which
# only Linux can build) it does not pass, and a failing run is not budgeted.
#
#   tier        measured (lines / bytes)                          budget
#   clippy      94 / 3,037 cold, 1 / 70 warm                      150 / 5,000
#   unit        744 / 47,145 cold (Mac), 653 / 44,386 (CI Linux)  1,000 / 64,000
#   unit-debug  723 / 46,497 cold (Mac), 674 / 46,723 (CI Linux)  1,000 / 64,000
#   mkfs        115 / 3,858 cold (Mac), 24 / 1,022 warm           160 / 5,200
#   suite       2,143 / 108,930 (CI Linux, fixtures built)        2,900 / 150,000
#   asan        790 / 52,350 (CI, nightly)                        1,100 / 72,000
#   scripts     46 / 979 (three shell tests)                       70 / 1,600
#   matrix      633 / 35,311 green, 1,521 / 78,075 red (see below)  900 / 50,000
#
# THE MATRIX ROW is measured on a GREEN 46-scenario run (2026-09-18, 46 min,
# max_parallel=4): 633 lines / 35,311 bytes, budgeted at that plus a third.
# Only a passing run is budgeted, so the 1,521 lines the same matrix printed
# when ten scenarios failed and were retried five times each do not have to
# fit -- a failing run prints its tail and fails on its own account. It is the
# only tier whose length depends on a machine rather than on this repository,
# so it is the one most likely to need raising -- do that with a measurement.
case "$TIER" in
    clippy)     MAX_LINES=150;  MAX_BYTES=5000 ;;
    unit)       MAX_LINES=1000; MAX_BYTES=64000 ;;
    unit-debug) MAX_LINES=1000; MAX_BYTES=64000 ;;
    mkfs)       MAX_LINES=160;  MAX_BYTES=5200 ;;
    suite)      MAX_LINES=2900; MAX_BYTES=150000 ;;
    asan)       MAX_LINES=1100; MAX_BYTES=72000 ;;
    scripts)    MAX_LINES=70;   MAX_BYTES=1600 ;;
    matrix)     MAX_LINES=900;  MAX_BYTES=50000 ;;
    *)
        echo "tier.sh: '$TIER' has no budget. Add a measured row to scripts/tier.sh." >&2
        exit 2
        ;;
esac

if [ ! -x "$BUDGET" ]; then
    echo "tier.sh: $BUDGET is missing." >&2
    echo "         The harness is a pinned sibling -- run 'chore siblings'." >&2
    exit 1
fi

# `chore test:unit -- --verbose` arrives as CLI_ARGS. output-budget.sh reads
# FWTH_VERBOSE itself, so mapping the flag onto it is all that is needed --
# and the variable and the flag cannot disagree.
case " ${CLI_ARGS:-} " in
    *" --verbose "*|*" -v "*) export FWTH_VERBOSE=1 ;;
esac

exec "$BUDGET" \
    --log "$REPO/tmp/logs/$TIER.log" \
    --max-lines "$MAX_LINES" \
    --max-bytes "$MAX_BYTES" \
    --label "$TIER" \
    -- "$@"
