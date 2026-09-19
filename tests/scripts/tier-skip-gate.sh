#!/usr/bin/env bash
# Tests for scripts/tier.sh's skip gate: the three exit statuses it must
# tell apart, on a real run through the harness's output-budget.sh.
#
# It failed every clean run when first written -- `set -o pipefail` plus a
# grep that matches nothing -- which is the bug these cases exist to keep
# out. A gate that fails everything is indistinguishable from a gate that
# works, until someone has a green run to compare against.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT" || exit 1

pass=0; fail=0

check() {
    local name="$1" want="$2"; shift 2
    bash scripts/tier.sh unit -- "$@" >/dev/null 2>&1
    local got=$?
    if [ "$got" = "$want" ]; then
        pass=$((pass + 1)); printf '  ok    %s\n' "$name"
    else
        fail=$((fail + 1)); printf '  FAIL  %s\n        got exit %s, want %s\n' "$name" "$got" "$want"
    fi
}

printf 'tier.sh skip gate\n'

# A passing run passes. This is the case the first cut broke.
check "a clean run"            0  bash -c 'echo "test result: ok. 1 passed"'
# A test that printed SKIP was counted as passing by libtest; 66 is the
# gate's own status, told apart from the suite's.
check "a run that skipped"    66  bash -c 'echo "SKIP: fixture absent"; echo "test result: ok. 1 passed"'
# A failing suite keeps ITS status, not the gate's.
check "a failing run"          3  bash -c 'echo boom; exit 3'
# Ignored tests are reported, not fatal: they are visible in the count.
check "ignored but no skips"   0  bash -c 'echo "test result: ok. 1 passed; 0 failed; 2 ignored"'
# SKIP must be anchored: a test whose OUTPUT mentions the word is not a skip.
check "the word in a message"  0  bash -c 'echo "we do not SKIP: anything"; echo "test result: ok. 1 passed"'

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
