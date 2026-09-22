#!/usr/bin/env bash
# Contract tests for tier.sh's Cargo-resolved transient output-budget adapter.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TIER="$ROOT/scripts/tier.sh"

pass=0
fail=0
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

printf 'output-budget adapter\n'

check() {
    description="$1"; shift
    if "$@"; then
        pass=$((pass + 1)); printf '  ok    %s\n' "$description"
    else
        fail=$((fail + 1)); printf '  FAIL  %s\n' "$description"
    fi
}

no_copies() {
    ! compgen -G "$ROOT/tmp/output-budget.*.sh" >/dev/null
}

# A minimal stand-in isolates the adapter contract from core's independently
# tested budgeting behavior. It records that the transient copy ran, then
# executes only the command following `--`.
core="$tmp/core"
bin="$tmp/bin"
mkdir -p "$core/scripts" "$bin"
cat > "$core/scripts/output-budget.sh" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$0" > "$ADAPTER_RECORD"
while [ "$1" != -- ]; do shift; done
shift
"$@"
EOF
chmod +x "$core/scripts/output-budget.sh"

cat > "$bin/cargo" <<'EOF'
#!/usr/bin/env bash
python3 - "$CORE_MANIFEST" <<'PY'
import json, sys
print(json.dumps({"packages": [{"name": "am-fs-core", "manifest_path": sys.argv[1]}]}))
PY
EOF
chmod +x "$bin/cargo"

ADAPTER_RECORD="$tmp/ran" CORE_MANIFEST="$core/Cargo.toml" PATH="$bin:$PATH" \
    bash "$TIER" unit -- true >"$tmp/success.out" 2>"$tmp/success.err"
success_status=$?
check 'Cargo-resolved copy runs' test "$success_status" -eq 0
check 'copy is transient on success' no_copies
check 'wrapper ran from tmp' grep -Eq '/tmp/output-budget\.[0-9]+\.sh$' "$tmp/ran"

ADAPTER_RECORD="$tmp/failed" CORE_MANIFEST="$core/Cargo.toml" PATH="$bin:$PATH" \
    bash "$TIER" unit -- bash -c 'exit 37' >"$tmp/command.out" 2>"$tmp/command.err"
command_status=$?
check 'wrapped command status is preserved' test "$command_status" -eq 37
check 'copy is transient after command failure' no_copies

cat > "$bin/cargo" <<'EOF'
#!/usr/bin/env bash
exit 42
EOF
ADAPTER_RECORD="$tmp/unreached" PATH="$bin:$PATH" \
    bash "$TIER" unit -- true >"$tmp/metadata.out" 2>"$tmp/metadata.err"
metadata_status=$?
check 'metadata failure uses adapter status' test "$metadata_status" -eq 1
cat > "$tmp/metadata.expected" <<'EOF'
tier.sh: cargo could not say where am-fs-core is, or its copy has no
         scripts/output-budget.sh. The wrapper lives in rust-fs-core;
         check the am-fs-core dependency resolves and is at a version
         that ships it (v0.2.11 or later).
EOF
check 'metadata failure is exactly the concise adapter diagnostic' \
    cmp -s "$tmp/metadata.expected" "$tmp/metadata.err"
check 'metadata failure creates no copy' no_copies

cat > "$bin/cargo" <<'EOF'
#!/usr/bin/env bash
printf '{"packages":[{"name":"am-fs-core","manifest_path":"%s"}]}\n' "$CORE_MANIFEST"
EOF
missing="$tmp/core-without-wrapper"
mkdir -p "$missing"
CORE_MANIFEST="$missing/Cargo.toml" PATH="$bin:$PATH" \
    bash "$TIER" unit -- true >"$tmp/missing.out" 2>"$tmp/missing.err"
missing_status=$?
check 'missing wrapper uses adapter status' test "$missing_status" -eq 1
check 'missing wrapper names the required script' \
    grep -q 'scripts/output-budget.sh' "$tmp/missing.err"
check 'missing wrapper creates no copy' no_copies

real_cp="$(command -v cp)"
cat > "$bin/cp" <<EOF
#!/usr/bin/env bash
"$real_cp" "\$1" "\$2"
exit 73
EOF
chmod +x "$bin/cp"
CORE_MANIFEST="$core/Cargo.toml" PATH="$bin:$PATH" \
    bash "$TIER" unit -- true >"$tmp/copy.out" 2>"$tmp/copy.err"
copy_status=$?
check 'copy failure status is preserved' test "$copy_status" -eq 73
check 'partial copy is removed' no_copies

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
