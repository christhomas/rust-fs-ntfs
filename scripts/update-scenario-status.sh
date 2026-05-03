#!/usr/bin/env bash
# update-scenario-status.sh -- atomically update a scenario's status.
#
# Usage:
#   bash scripts/update-scenario-status.sh <scenario-name> <new-status> [<evidence-link>]
#
# Examples:
#   bash scripts/update-scenario-status.sh mac-format-basic-256mib passed-agent-3f7c-2026-05-02
#   bash scripts/update-scenario-status.sh mac-format-tiny-32mib failed-agent-3f7c-2026-05-02 "$DIAG_DIR/iter-..."
#   bash scripts/update-scenario-status.sh mac-format-cluster-512 blocked-needs-evidence-agent-3f7c-2026-05-02
#
# Atomicity: same mktemp + mv pattern as claim-scenario.sh.

set -euo pipefail

SCENARIO="${1:-}"
NEW_STATUS="${2:-}"
EVIDENCE="${3:-}"
if [[ -z "${SCENARIO}" || -z "${NEW_STATUS}" ]]; then
    echo "usage: $0 <scenario-name> <new-status> [<evidence-link>]" >&2
    exit 2
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK_LIST="${REPO_ROOT}/test-matrix.json"
TMP="$(mktemp "${WORK_LIST}.tmp.XXXXXX")"

python3 - "${WORK_LIST}" "${TMP}" "${SCENARIO}" "${NEW_STATUS}" "${EVIDENCE}" <<'PY'
import json, sys
src, dst, scenario, new_status, evidence = sys.argv[1:6]
with open(src) as f:
    data = json.load(f)
if scenario not in data.get("scenarios", {}):
    print(f"unknown scenario: {scenario}", file=sys.stderr)
    sys.exit(2)
data["scenarios"][scenario]["status"] = new_status
if evidence:
    data["scenarios"][scenario]["evidence_link"] = evidence
with open(dst, "w") as f:
    json.dump(data, f, indent=2, ensure_ascii=False)
PY

mv "${TMP}" "${WORK_LIST}"
echo "${SCENARIO} -> ${NEW_STATUS}"
