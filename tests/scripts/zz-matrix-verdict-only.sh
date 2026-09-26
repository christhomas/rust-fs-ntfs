#!/usr/bin/env bash
# A fetched verdict must be graded even when the VM produced no chkdsk text.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/repo/scripts" "$tmp/repo/test-diagnostics/matrix/verdict-only" "$tmp/bin"
ln -s "$ROOT/scripts/matrix-fetch-diag.sh" "$tmp/repo/scripts/matrix-fetch-diag.sh"
ln -s "$ROOT/scripts/vm-ssh-agent.sh" "$tmp/repo/scripts/vm-ssh-agent.sh"
printf '{"steps":[{"host":"vm"}]}\n' > "$tmp/repo/test-diagnostics/matrix/verdict-only/recipe.json"
printf '%s\n' '{"passed":true,"verdict_shape":"clean","modes":{"/scan":{"exit":3,"state":"failed","reason":"errors"}}}' > "$tmp/verdict.json"

# Stand in for the VM transfer; the scenario has only verdict.json.
# shellcheck disable=SC2016 # These variables expand in the generated script.
printf '#!/usr/bin/env bash\ncp "$MOCK_VERDICT" "${@: -1}/verdict.json"\n' > "$tmp/bin/scp"
chmod +x "$tmp/bin/scp"

got="$(PATH="$tmp/bin:$PATH" MOCK_VERDICT="$tmp/verdict.json" VM_HOST=vm VM_WORKDIR=C:/work bash "$tmp/repo/scripts/matrix-fetch-diag.sh")"
want=$'verdict-only: files: verdict.json ; verdict: mismatch: /scan=failed\ndiag: 1 scenario(s), 1 with something to look at -- test-diagnostics/matrix/<scenario>/vm/, summary.txt'
if [ "$got" != "$want" ]; then
    printf 'FAIL verdict-only summary\ngot: %s\nwant: %s\n' "$got" "$want" >&2
    exit 1
fi
if [ "$(cat "$tmp/repo/test-diagnostics/matrix/summary.txt")" != "${want%%$'\n'*}" ]; then
    echo 'FAIL verdict-only summary.txt' >&2
    exit 1
fi

printf '%s\n' '{"passed":true,"verdict_shape":"clean","modes":{"/scan":{"exit":0,"state":"scanned","reason":"ok"}}}' > "$tmp/verdict.json"
got="$(PATH="$tmp/bin:$PATH" MOCK_VERDICT="$tmp/verdict.json" VM_HOST=vm VM_WORKDIR=C:/work bash "$tmp/repo/scripts/matrix-fetch-diag.sh")"
want='diag: 1 scenario(s), 0 with something to look at -- test-diagnostics/matrix/<scenario>/vm/, summary.txt'
if [ "$got" != "$want" ]; then
    printf 'FAIL matching verdict-only summary\ngot: %s\nwant: %s\n' "$got" "$want" >&2
    exit 1
fi
