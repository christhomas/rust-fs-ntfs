#!/usr/bin/env bash
# Exercise the exact verdict command that CI runs, including missing/skipped
# dependencies. A green aggregate must never launder an absent gate.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
pass=0
fail=0

check() {
    local name="$1" mode="$2" want="$3" json="$4" got
    GATE_NEEDS_JSON="$json" bash "$ROOT/scripts/ci-verdict.sh" "$mode" >/dev/null 2>&1
    got=$?
    if { [ "$want" = pass ] && [ "$got" -eq 0 ]; } ||
       { [ "$want" = fail ] && [ "$got" -ne 0 ]; }; then
        pass=$((pass + 1))
    else
        fail=$((fail + 1))
        printf 'FAIL %s: exit %s, expected %s\n' "$name" "$got" "$want"
    fi
}

windows_true='{"changes":{"result":"success","outputs":{"mkfs":"true"}},"validate-mkfs-windows-run":{"result":"success"}}'
windows_false='{"changes":{"result":"success","outputs":{"mkfs":"false"}},"validate-mkfs-windows-run":{"result":"skipped"}}'
aggregate='{"test":{"result":"success"},"integration":{"result":"success"},"changes":{"result":"success"},"validate-mkfs-windows":{"result":"success"}}'

check windows-required-ran windows pass "$windows_true"
check windows-unneeded-skipped windows pass "$windows_false"
check windows-required-skipped windows fail "$(jq -c '.["validate-mkfs-windows-run"].result="skipped"' <<< "$windows_true")"
check windows-unneeded-failed windows fail "$(jq -c '.["validate-mkfs-windows-run"].result="failure"' <<< "$windows_false")"
check windows-filter-failed windows fail "$(jq -c '.changes.result="failure"' <<< "$windows_true")"
check windows-missing-run windows fail '{"changes":{"result":"success","outputs":{"mkfs":"true"}}}'
check windows-missing-decision windows fail '{"changes":{"result":"success"},"validate-mkfs-windows-run":{"result":"skipped"}}'

check aggregate-green aggregate pass "$aggregate"
for job in test integration changes validate-mkfs-windows; do
    for result in failure cancelled skipped; do
        check "aggregate-$job-$result" aggregate fail \
            "$(jq -c --arg job "$job" --arg result "$result" '.[$job].result=$result' <<< "$aggregate")"
    done
done
check aggregate-missing-integration aggregate fail '{"test":{"result":"success"},"changes":{"result":"success"},"validate-mkfs-windows":{"result":"success"}}'
check aggregate-invalid-json aggregate fail '{'

printf 'ci verdict: %d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
