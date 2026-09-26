#!/usr/bin/env bash
# tier.sh TIER -- COMMAND [ARG...]
#
# One test tier, run QUIETLY and under a budget. The whole run goes to
# tmp/logs/<TIER>.log; a pass prints one verdict line naming the log, a
# failure prints the tail of it, and a run that passed but printed more than
# its budget fails with status 65 -- told apart from a failing suite, which
# exits with the suite's own status.
#
# The work is done by rust-fs-core's canonical scripts/output-budget.sh. This
# file asks cargo where core is, copies that script for the run, and owns only
# the part that is ours: which tiers exist and how much each may print.
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
# VERBOSE. `OUTPUT_BUDGET_VERBOSE=1`, or `--verbose`/`-v` in a chore
# invocation's CLI_ARGS (`chore test:unit -- --verbose`), streams the run as it
# happens as well as logging it. It does NOT lift the budget: the log is the
# same size either way.
#
# THE VARIABLE WAS `FWTH_VERBOSE` while the wrapper was the Windows harness's.
# The canonical script does not read that name, and a rename like this fails
# silently -- nothing errors, the run simply stays quiet. Core names the
# replacement on stderr when it sees an `FWTH_*` or `FLTH_*` variable; nothing
# else does, which is why it is written down here too.
#
# A FAILING TIER IS QUIET TOO, from core v0.2.13: the verdict, the exit status
# and the log's path, not forty lines of tail. `--tail N`, or
# OUTPUT_BUDGET_FAIL_TAIL=N, brings the tail back for whoever is watching.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

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
#   clippy      196 / 8,654 (CI Linux, 2026-09-19)                260 / 11,500
#   unit        744 / 47,145 cold (Mac), 653 / 44,386 (CI Linux)  1,000 / 64,000
#   unit-debug  723 / 46,497 cold (Mac), 674 / 46,723 (CI Linux)  1,000 / 64,000
#   mkfs        115 / 3,858 cold (Mac), 24 / 1,022 warm           160 / 5,200
#   suite       2,143 / 108,930 (CI Linux, fixtures built)        2,900 / 150,000
#   asan        790 / 52,350 (CI, nightly)                        1,100 / 72,000
#   scripts     101 / 2,669 (11 shell tests, rebased 2026-09-26)  135 / 3,500
#   matrix      633 / 35,311 green, 1,521 / 78,075 red (see below)  900 / 50,000
#
# THE CLIPPY ROW MOVED ON 2026-09-19, from 150/5,000 to 260/11,500. Adding
# tests/fuzz_decoders.rs gave `--all-targets` another target to lint, and a
# cold clippy prints a line per crate compiled: the measured figure went from
# 94 lines to 196 on CI (run 35448280025, job 105910912877). Raised to that
# plus a third, by the same rule as every other row. This is the "raise the
# budget deliberately" the failure message asks for -- the run passed, it
# simply printed more than the old measurement allowed.

# THE SCRIPTS ROW MOVED ON 2026-09-26, from 80/1,800 to 110/2,500.
# `chore test:scripts` on fix/diagnostics-verdict-shape at b815f12 printed
# 82 lines / 1,831 bytes from six shell tests, including 13 new verdict
# cases in tests/scripts/matrix-fetch-diag.sh. Both limits are that passing
# measurement plus roughly a third; every test verdict remains in the log.
# The combined shell suite, including the CI PR-trigger and routed-path
# regression tests, measured 101 lines and 2,669 bytes after rebase on
# 2026-09-26. The cap is 135 lines / 3,500 bytes, about one third of headroom.

# THE MATRIX ROW is measured on a GREEN 46-scenario run (2026-09-18, 46 min,
# max_parallel=4): 633 lines / 35,311 bytes, budgeted at that plus a third.
# Only a passing run is budgeted, so the 1,521 lines the same matrix printed
# when ten scenarios failed and were retried five times each do not have to
# fit -- a failing run prints its tail and fails on its own account. It is the
# only tier whose length depends on a machine rather than on this repository,
# so it is the one most likely to need raising -- do that with a measurement.
case "$TIER" in
    clippy)     MAX_LINES=260;  MAX_BYTES=11500 ;;
    unit)       MAX_LINES=1000; MAX_BYTES=64000 ;;
    unit-debug) MAX_LINES=1000; MAX_BYTES=64000 ;;
    mkfs)       MAX_LINES=160;  MAX_BYTES=5200 ;;
    suite)      MAX_LINES=2900; MAX_BYTES=150000 ;;
    asan)       MAX_LINES=1100; MAX_BYTES=72000 ;;
    scripts)    MAX_LINES=135;  MAX_BYTES=3500 ;;
    matrix)     MAX_LINES=900;  MAX_BYTES=50000 ;;
    *)
        echo "tier.sh: '$TIER' has no budget. Add a measured row to scripts/tier.sh." >&2
        exit 2
        ;;
esac

# THE WRAPPER IS COPIED FROM rust-fs-core FOR THIS RUN, AND DELETED AFTER IT.
#
# It belongs to core, and it is deliberately NOT committed here. A committed
# copy is a copy that drifts: measured on 2026-09-22 the family had three of
# them, reached four different ways, each repository internally consistent
# and nothing comparing them.
#
# CARGO IS ASKED WHERE CORE IS, rather than this script guessing. Cargo has
# already resolved the dependency, and its answer is right in both shapes
# this family uses: with `path = "../rust-fs-core"` it reports the developer's
# own checkout, so work in progress on the wrapper is exercised here on the
# next run; with a plain version requirement it reports the registry copy of
# the pinned release. There is no sibling-versus-crate decision to make,
# because cargo made it.
#
# Compilation flags belong to the wrapped command, not this discovery probe.
# In particular the ASan tier supplies nightly-only -Z flags while this plain
# `cargo metadata` uses the default stable toolchain. Clearing them here keeps
# discovery toolchain-neutral; the command below still inherits them intact.
#
# tmp/ is gitignored and is where the tier logs already live.
set +e
CORE_DIR="$(RUSTFLAGS= RUSTDOCFLAGS= \
    cargo metadata --format-version 1 --locked --manifest-path "$REPO/Cargo.toml" \
    2>/dev/null | python3 -c '
import json, sys
packages = json.load(sys.stdin)["packages"]
print(next((p["manifest_path"].rsplit("/", 1)[0]
            for p in packages if p["name"] == "am-fs-core"), ""))
' 2>/dev/null)"
metadata_status=$?
set -e
if [ "$metadata_status" -ne 0 ] || [ -z "$CORE_DIR" ] || [ ! -f "$CORE_DIR/scripts/output-budget.sh" ]; then
    echo "tier.sh: cargo could not say where am-fs-core is, or its copy has no" >&2
    echo "         scripts/output-budget.sh. The wrapper lives in rust-fs-core;" >&2
    echo "         check the am-fs-core dependency resolves and is at a version" >&2
    echo "         that ships it (v0.2.13 or later)." >&2
    exit 1
fi

# WHAT CARGO POINTED AT IS ASKED TO IDENTIFY ITSELF. Resolution finding *a*
# file at that path is not the same as finding CORE's wrapper: a stale vendor
# directory, a half-written override or a package that renamed the script all
# produce a path that exists and does not behave. `--version` is the contract
# the script publishes for exactly this, and it is what the resolver this
# replaced used to check.
#
# A DIGEST IS DELIBERATELY NOT CHECKED. The earlier resolver pinned the
# script's SHA-256, which meant every comment core added to it broke this
# repository until the digest was chased; the same pin in seven consumers is
# the lockstep the move to one canonical copy exists to remove.
EXPECTED_API="rust-fs-core-output-budget 1"
if [ "$(bash "$CORE_DIR/scripts/output-budget.sh" --version 2>/dev/null || true)" != "$EXPECTED_API" ]; then
    echo "tier.sh: $CORE_DIR/scripts/output-budget.sh is there, but does not" >&2
    echo "         answer --version with '$EXPECTED_API'. That is a broken or" >&2
    echo "         far too old rust-fs-core, not an absent one." >&2
    exit 1
fi

BUDGET="$REPO/tmp/output-budget.$$.sh"
mkdir -p "$REPO/tmp"
trap 'rm -f "$BUDGET"' EXIT
cp "$CORE_DIR/scripts/output-budget.sh" "$BUDGET"

# `chore test:unit -- --verbose` arrives as CLI_ARGS. output-budget.sh reads
# OUTPUT_BUDGET_VERBOSE itself, so mapping the flag onto it is all that is
# needed -- and the variable and the flag cannot disagree.
case " ${CLI_ARGS:-} " in
    *" --verbose "*|*" -v "*) export OUTPUT_BUDGET_VERBOSE=1 ;;
esac

LOG="$REPO/tmp/logs/$TIER.log"

set +e
bash "$BUDGET" \
    --log "$LOG" \
    --max-lines "$MAX_LINES" \
    --max-bytes "$MAX_BYTES" \
    --label "$TIER" \
    -- "$@"
status=$?
set -e

# A TEST THAT DECIDED NOT TO RUN IS NOT A TEST THAT PASSED. Two shapes
# reach the log and neither turns a run red on its own:
#
#   * `SKIP: ...` on stderr from a test that found its fixture absent and
#     returned early -- it is counted as passed;
#   * libtest's own `N ignored`, from `#[ignore]`.
#
# Both are reported here and the first one fails the tier. Fixture-driven
# tests require their images and panic when one is missing.
if [ -f "$LOG" ]; then
    # `|| true` on BOTH, and for two different reasons under `set -o
    # pipefail`: grep exits 1 when it matches nothing, which is the
    # ordinary case here, and in the second the failing grep is upstream
    # of awk in a pipeline, so pipefail propagates it. Without these the
    # gate failed every clean run -- measured, not imagined.
    skips=$(grep -ac '^SKIP:' "$LOG" || true)
    ignored=$( (grep -aoE '[0-9]+ ignored' "$LOG" || true) | awk '{s+=$1} END{print s+0}')
    if [ "${ignored:-0}" -gt 0 ]; then
        echo "$TIER: $ignored test(s) ignored -- see #277"
    fi
    if [ "${skips:-0}" -gt 0 ]; then
        echo "::error::$TIER: $skips test(s) printed SKIP and were counted as passing. A skipped test is not a passing test; see $LOG" >&2
        grep -a '^SKIP:' "$LOG" | sed 's/^/    /' >&2
        exit 66
    fi
fi

exit "$status"
