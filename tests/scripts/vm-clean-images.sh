#!/usr/bin/env bash
# Tests for clean_says in scripts/vm-clean-images.sh -- the function that
# turns the three numbers the VM reports into the line a person reads.
#
# The sweep itself needs a Windows VM; this does not. What can be wrong here
# is the wording and the arithmetic: a sweep that removed nothing must not
# claim it removed something, and bytes must be reported as GiB.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# shellcheck disable=SC1091
. "$ROOT/scripts/vm-clean-images.sh"

pass=0
fail=0

check() {
    local name="$1" input="$2" want="$3" got
    got="$(printf '%s\n' "$input" | clean_says)"
    if [ "$got" = "$want" ]; then
        printf '  ok    %s\n' "$name"
        pass=$((pass + 1))
    else
        printf '  FAIL  %s\n        got:  %s\n        want: %s\n' "$name" "$got" "$want"
        fail=$((fail + 1))
    fi
}

check "nothing to remove" \
    "0 0 58000000000" \
    "vm:clean: no images left behind — C: has 54 GiB free"

check "one image" \
    "1 1073741824 58000000000" \
    "vm:clean: removed 1 image file(s), 1 GiB — C: has 54 GiB free"

# The run that prompted this script: 51 files, 30.7 GiB, C: at 23 GiB free.
check "the crashed run's wreckage" \
    "51 32963298918 24696061952" \
    "vm:clean: removed 51 image file(s), 30 GiB — C: has 23 GiB free"

# Files that exist but total less than a GiB still get reported as removed:
# the count is the fact, the size is the detail.
check "sub-gibibyte total" \
    "3 1048576 58000000000" \
    "vm:clean: removed 3 image file(s), 0 GiB — C: has 54 GiB free"

# A VM that answers with a blank line must not be read as "0 files removed
# and 0 bytes free", which would look like a full disk.
check "no numbers at all" \
    "" \
    "vm:clean: no images left behind — C: has 0 GiB free"

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
