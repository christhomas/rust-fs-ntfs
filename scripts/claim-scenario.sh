#!/usr/bin/env bash
# claim-scenario.sh -- atomic scenario claim from tests/matrix/work-list.json.
#
# Usage:
#   bash scripts/claim-scenario.sh "<session-name>"
#
# Output:
#   stdout: the claimed scenario name (one line) on success
#   exit 0 -- scenario claimed
#   exit 1 -- no pending scenarios available
#   exit 2 -- usage error / missing work list
#
# Atomicity:
#   The work list is rewritten via mktemp + mv, which is atomic on POSIX.
#   To prevent concurrent agents from picking the same scenario:
#     1. Two agents may BOTH compute "first pending = X" simultaneously.
#     2. They both atomically rewrite the work list with claim X.
#     3. Whichever rename arrives last wins -- the other's data is gone.
#   So we read back AFTER the rename and verify our session won. If the
#   read shows another session as the claimer, we retry with the next
#   scenario.
#
# Conservative: we do at most 16 claim attempts before giving up.

set -euo pipefail

SESSION="${1:-}"
if [[ -z "${SESSION}" ]]; then
    echo "usage: $0 <session-name>" >&2
    exit 2
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK_LIST="${REPO_ROOT}/tests/matrix/work-list.json"

if [[ ! -f "${WORK_LIST}" ]]; then
    echo "missing work list: ${WORK_LIST}" >&2
    exit 2
fi

claim_attempt() {
    local session="$1"
    local tmp
    tmp="$(mktemp "${WORK_LIST}.tmp.XXXXXX")"

    # Pick the first pending scenario, set its status to claimed-<session>.
    # If no pending scenarios remain, exit 1.
    local picked
    picked="$(python3 - "${WORK_LIST}" "${tmp}" "${session}" <<'PY'
import json, sys
src, dst, session = sys.argv[1], sys.argv[2], sys.argv[3]
with open(src) as f:
    data = json.load(f)
picked = None
for name, entry in data.get("scenarios", {}).items():
    if entry.get("status") == "pending":
        picked = name
        entry["status"] = f"claimed-{session}"
        break
if picked is None:
    sys.exit(1)
with open(dst, "w") as f:
    json.dump(data, f, indent=2, ensure_ascii=False)
print(picked)
PY
    )" || { rm -f "${tmp}"; return 1; }

    # Atomic replace.
    mv "${tmp}" "${WORK_LIST}"

    # Read back and verify our claim won.
    local actual_status
    actual_status="$(python3 -c "
import json, sys
with open('${WORK_LIST}') as f:
    data = json.load(f)
print(data['scenarios']['${picked}']['status'])
")"
    if [[ "${actual_status}" == "claimed-${session}" ]]; then
        echo "${picked}"
        return 0
    fi
    # Another agent overwrote us; signal retry.
    return 2
}

for attempt in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16; do
    if claim_attempt "${SESSION}"; then
        exit 0
    fi
    rc=$?
    if [[ ${rc} -eq 1 ]]; then
        # No pending scenarios.
        exit 1
    fi
    # Lost a race; randomised backoff before retry.
    sleep "$(awk "BEGIN { srand(); print rand() * 0.5 + 0.1 }")"
done

echo "claim-scenario.sh: 16 attempts failed -- treat as no scenarios available" >&2
exit 1
