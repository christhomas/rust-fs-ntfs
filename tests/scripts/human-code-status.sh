#!/usr/bin/env bash
# The status summary must agree with the explicit states in the body. The
# source report contains 56 High and 44 Medium findings; Low findings are not
# tracked by this ledger.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
STATUS="$ROOT/docs/human-code-status.md"

fixed_body=$(grep -Ec '^### .* — \*\*fixed( earlier)?\*\*$' "$STATUS")
documented_body=$(grep -Ec '^### .* — \*\*documented and pinned\*\*$' "$STATUS")
fixed_summary=$(awk -F'|' '$2 ~ /^ Fixed / { gsub(/ /, "", $3); print $3 }' "$STATUS")
documented_summary=$(awk -F'|' '$2 ~ /^ Documented and pinned/ { print $3 + 0 }' "$STATUS")
open_summary=$(awk -F'|' '$2 ~ /^ Still open / { print $3 + 0 }' "$STATUS")
open_heading=$(sed -n 's/^## Still open — \([0-9][0-9]*\) High and Medium$/\1/p' "$STATUS")
expected_open=$((56 + 44 - fixed_body - documented_body))

pass=0; fail=0
check_equal() {
    local name="$1" got="$2" want="$3"
    if [ "$got" = "$want" ]; then
        pass=$((pass + 1))
    else
        fail=$((fail + 1))
        printf '  FAIL  %s (got %s, wanted %s)\n' "$name" "$got" "$want"
    fi
}

printf 'human-code-status\n'
check_equal "fixed summary agrees with body" "$fixed_summary" "$fixed_body"
check_equal "documented summary agrees with body" "$documented_summary" "$documented_body"
check_equal "open summary is the remaining High/Medium findings" "$open_summary" "$expected_open"
check_equal "open heading agrees with summary" "$open_heading" "$open_summary"

if grep -Eq '^### B4 .*— \*\*partially fixed; same-length path tracked by \[#140\]' "$STATUS"; then
    pass=$((pass + 1))
else
    fail=$((fail + 1))
    printf '  FAIL  B4 must remain partial and link existing issue #140\n'
fi

printf '  %d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
