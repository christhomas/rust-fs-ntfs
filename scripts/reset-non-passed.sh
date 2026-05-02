#!/usr/bin/env bash
# reset-non-passed.sh -- pass-2/3 helper: reset every non-`passed-*`
# scenario in tests/matrix/work-list.json back to `pending` so a new
# pass can re-claim them. Idempotent.
#
# Usage: bash scripts/reset-non-passed.sh

set -euo pipefail
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK_LIST="${REPO_ROOT}/tests/matrix/work-list.json"

python3 - "${WORK_LIST}" <<'PY'
import json, sys
src = sys.argv[1]
with open(src) as f:
    d = json.load(f)
moved = 0
for name, e in d.get("scenarios", {}).items():
    s = e.get("status", "")
    if not s.startswith("passed-"):
        e["status"] = "pending"
        moved += 1
with open(src, "w") as f:
    json.dump(d, f, indent=2, ensure_ascii=False)
    f.write("\n")
print(f"reset {moved} scenarios to pending; {len(d['scenarios']) - moved} remain passed-*")
PY
