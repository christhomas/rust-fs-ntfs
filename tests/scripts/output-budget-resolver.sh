#!/usr/bin/env bash
# Contract tests for the consumer-side output-budget resolver.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RESOLVER="$ROOT/scripts/resolve-output-budget.sh"
CORE_ROOT="${FS_CORE_ROOT:-$ROOT/../rust-fs-core}"

pass=0
fail=0
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

printf 'output-budget resolver\n'

# The coordinated checkout path is available in the normal family workspace.
if [ -f "$CORE_ROOT/scripts/output-budget.sh" ]; then
    resolved="$(FS_CORE_ROOT="$CORE_ROOT" bash "$RESOLVER")"
    if [ "$resolved" = "$CORE_ROOT/scripts/output-budget.sh" ]; then
        pass=$((pass + 1)); printf '  ok    sibling source\n'
    else
        fail=$((fail + 1)); printf '  FAIL  sibling source\n        got: %s\n' "$resolved"
    fi
else
    printf '  ok    sibling source (not present; standalone mode)\n'
    pass=$((pass + 1))
fi

# Make Cargo report the real core package through metadata while forcing the
# resolver away from the sibling. This exercises the packaged-source branch
# without checking a second copy of the canonical script into this project.
if [ -f "$CORE_ROOT/scripts/output-budget.sh" ]; then
    fake_bin="$tmp/bin"
    mkdir -p "$fake_bin"
    printf '#!/usr/bin/env bash\npython3 - <<'"'"'PY'"'"'\nimport json\nimport os\nprint(json.dumps({"packages": [{"name": "am-fs-core", "manifest_path": os.environ["CORE_MANIFEST"]}]}))\nPY\n' \
        > "$fake_bin/cargo"
    chmod +x "$fake_bin/cargo"
    packaged="$(PATH="$fake_bin:$PATH" CORE_MANIFEST="$CORE_ROOT/Cargo.toml" FS_CORE_ROOT="$fake_bin/no-core" bash "$RESOLVER")"
    if [ "$packaged" = "$CORE_ROOT/scripts/output-budget.sh" ]; then
        pass=$((pass + 1)); printf '  ok    packaged Cargo source\n'
    else
        fail=$((fail + 1)); printf '  FAIL  packaged Cargo source\n        got: %s\n' "$packaged"
    fi
fi

# No sibling and no resolved package must be an actionable failure.
missing_root="$tmp/no-core"
if FS_CORE_ROOT="$missing_root" bash "$RESOLVER" >"$tmp/missing.out" 2>"$tmp/missing.err"; then
    fail=$((fail + 1)); printf '  FAIL  missing source fails\n        resolver unexpectedly passed\n'
else
    if grep -q 'no sibling or resolved Cargo package' "$tmp/missing.err"; then
        pass=$((pass + 1)); printf '  ok    missing source fails clearly\n'
    else
        fail=$((fail + 1)); printf '  FAIL  missing source fails clearly\n'
        sed 's/^/        /' "$tmp/missing.err"
    fi
fi

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
