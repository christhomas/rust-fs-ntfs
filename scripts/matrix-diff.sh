#!/usr/bin/env bash
# scripts/matrix-diff.sh — compare two matrix-results.json files and
# print a human-readable delta. Exit 0 iff no scenario regressed
# (was "ok", became "FAILED"); non-zero otherwise.
#
# Usage:
#   bash scripts/matrix-diff.sh OLD.json NEW.json
#
# Typical inputs:
#   OLD = test-diagnostics/matrix-results.json (the committed baseline)
#   NEW = test-diagnostics/matrix-results.json after a fresh run on a feature branch
#
# Exit codes:
#   0  — no regressions (new file may add scenarios, improve exit codes, etc.)
#   1  — at least one scenario regressed (ok → FAILED)
#   2  — invalid input

set -euo pipefail

if [ "$#" -ne 2 ]; then
    echo "usage: $0 OLD.json NEW.json" >&2
    exit 2
fi

old="$1"
new="$2"
for f in "$old" "$new"; do
    if [ ! -f "$f" ]; then
        echo "missing: $f" >&2
        exit 2
    fi
done

python3 - "$old" "$new" <<'PY'
import json
import sys

old = json.loads(open(sys.argv[1]).read())
new = json.loads(open(sys.argv[2]).read())

old_s = old.get("scenarios", {})
new_s = new.get("scenarios", {})

regressions = []        # ok → FAILED
fixed = []              # FAILED → ok
exit_drift = []         # exit code changed
new_scenarios = []
removed = []

for name in sorted(set(old_s) | set(new_s)):
    o = old_s.get(name)
    n = new_s.get(name)
    if o is None:
        new_scenarios.append(name)
        continue
    if n is None:
        removed.append(name)
        continue
    if o.get("status") == "ok" and n.get("status") == "FAILED":
        regressions.append(name)
    elif o.get("status") == "FAILED" and n.get("status") == "ok":
        fixed.append(name)
    o_exits = o.get("exits") or {}
    n_exits = n.get("exits") or {}
    diffs = []
    for k in sorted(set(o_exits) | set(n_exits)):
        if o_exits.get(k) != n_exits.get(k):
            diffs.append(f"{k}: {o_exits.get(k)} → {n_exits.get(k)}")
    if diffs and o.get("status") == n.get("status"):
        exit_drift.append((name, diffs))

# Header
old_sha = old.get("tested_at_sha", "?")[:7]
new_sha = new.get("tested_at_sha", "?")[:7]
print(f"matrix-diff: {old_sha} → {new_sha}")
print(f"  summary: old={old.get('summary', {})}")
print(f"  summary: new={new.get('summary', {})}")
print()

if regressions:
    print(f"REGRESSIONS ({len(regressions)}):")
    for n in regressions:
        print(f"  - {n}")
    print()
if fixed:
    print(f"FIXED ({len(fixed)}):")
    for n in fixed:
        print(f"  + {n}")
    print()
if exit_drift:
    print(f"EXIT DRIFT ({len(exit_drift)}):")
    for n, ds in exit_drift:
        print(f"  ~ {n}: {', '.join(ds)}")
    print()
if new_scenarios:
    print(f"NEW SCENARIOS ({len(new_scenarios)}):")
    for n in new_scenarios:
        print(f"  + {n}")
    print()
if removed:
    print(f"REMOVED SCENARIOS ({len(removed)}):")
    for n in removed:
        print(f"  - {n}")
    print()

if not (regressions or fixed or exit_drift or new_scenarios or removed):
    print("no deltas")

# Exit 1 on regression
sys.exit(1 if regressions else 0)
PY
