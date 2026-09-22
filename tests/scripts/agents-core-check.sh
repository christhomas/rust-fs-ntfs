#!/usr/bin/env bash
# Tests for scripts/agents-core-check.sh: it must reject a modified, stale or
# absent shared block, not merely accept a good one.
#
# A gate that cannot fail is indistinguishable from no gate. The first version
# of these cases reported every failure as a pass, because `$?` after a pipe
# is the exit status of the pipe's last command -- which is the same mistake
# #306 was opened for.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT" || exit 1

pass=0; fail=0
BAK="$(mktemp)"; cp AGENTS.md "$BAK"
restore() { cp "$BAK" AGENTS.md; }
trap 'restore; rm -f "$BAK"' EXIT

check() {
    local name="$1" want="$2"
    bash scripts/agents-core-check.sh >/dev/null 2>&1
    local got=$?
    local ok; [ "$want" = zero ] && ok=$([ $got -eq 0 ] && echo y) || ok=$([ $got -ne 0 ] && echo y)
    if [ "$ok" = y ]; then pass=$((pass + 1))
    else fail=$((fail + 1)); printf '  FAIL  %s (exit %s, wanted %s)\n' "$name" "$got" "$want"; fi
}

printf 'agents-core-check\n'
check "a clean file passes" zero
sed -i.t 's/lexically lowest/lexically highest/' AGENTS.md && rm -f AGENTS.md.t
check "a word changed inside the block" nonzero; restore
grep -v 'BEGIN SHARED BLOCK' "$BAK" > AGENTS.md
check "a missing BEGIN marker" nonzero; restore
grep -v 'END SHARED BLOCK' "$BAK" > AGENTS.md
check "a missing END marker" nonzero; restore
sed -i.t 's/sha256:60fad6dd/sha256:00000000/' AGENTS.md && rm -f AGENTS.md.t
check "a marker that disagrees with the content" nonzero; restore
rm -f AGENTS.md
check "no AGENTS.md at all" nonzero; restore

printf '  %d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
