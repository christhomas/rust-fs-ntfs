#!/usr/bin/env bash
# Contract tests for tier.sh's Cargo-resolved transient output-budget adapter.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TIER="$ROOT/scripts/tier.sh"

pass=0
fail=0
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# When this contract runs inside `chore test:scripts`, the outer scripts tier
# legitimately owns one live transient copy. Record that baseline so each
# adapter invocation is required to leave no additional copy behind.
find "$ROOT/tmp" -maxdepth 1 -name 'output-budget.*.sh' -print 2>/dev/null \
    | sort > "$tmp/copies.before"

printf 'output-budget adapter\n'

check() {
    description="$1"; shift
    if "$@"; then
        pass=$((pass + 1)); printf '  ok    %s\n' "$description"
    else
        fail=$((fail + 1)); printf '  FAIL  %s\n' "$description"
    fi
}

no_new_copies() {
    find "$ROOT/tmp" -maxdepth 1 -name 'output-budget.*.sh' -print 2>/dev/null \
        | sort > "$tmp/copies.after"
    cmp -s "$tmp/copies.before" "$tmp/copies.after"
}

# A minimal stand-in isolates the adapter contract from core's independently
# tested budgeting behavior. It records that the transient copy ran, then
# executes only the command following `--`.
core="$tmp/core"
bin="$tmp/bin"
mkdir -p "$core/scripts" "$bin"
cat > "$core/scripts/output-budget.sh" <<'EOF'
#!/usr/bin/env bash
# --version is answered first and exits, because the adapter asks the
# resolved script to identify itself before it copies or runs anything.
[ "${1-}" = --version ] && { echo 'rust-fs-core-output-budget 1'; exit 0; }
printf '%s\n' "$0" > "$ADAPTER_RECORD"
printf '%s\n%s\n' "${RUSTFLAGS-}" "${RUSTDOCFLAGS-}" > "$ADAPTER_ENV_RECORD"
while [ "$1" != -- ]; do shift; done
shift
"$@"
EOF
chmod +x "$core/scripts/output-budget.sh"

cat > "$bin/cargo" <<'EOF'
#!/usr/bin/env bash
[ -z "${RUSTFLAGS-}" ] && [ -z "${RUSTDOCFLAGS-}" ] || exit 86
python3 - "$CORE_MANIFEST" <<'PY'
import json, sys
print(json.dumps({"packages": [{"name": "am-fs-core", "manifest_path": sys.argv[1]}]}))
PY
EOF
chmod +x "$bin/cargo"

ADAPTER_RECORD="$tmp/ran" ADAPTER_ENV_RECORD="$tmp/env" \
    CORE_MANIFEST="$core/Cargo.toml" PATH="$bin:$PATH" \
    RUSTFLAGS='metadata-must-not-see-rustflags' \
    RUSTDOCFLAGS='metadata-must-not-see-rustdocflags' \
    bash "$TIER" unit -- true >"$tmp/success.out" 2>"$tmp/success.err"
success_status=$?
check 'Cargo-resolved copy runs' test "$success_status" -eq 0
check 'copy is transient on success' no_new_copies
check 'wrapper ran from tmp' grep -Eq '/tmp/output-budget\.[0-9]+\.sh$' "$tmp/ran"
printf '%s\n%s\n' 'metadata-must-not-see-rustflags' \
    'metadata-must-not-see-rustdocflags' > "$tmp/env.expected"
check 'wrapped command retains compile flags' cmp -s "$tmp/env.expected" "$tmp/env"

ADAPTER_RECORD="$tmp/failed" ADAPTER_ENV_RECORD="$tmp/failed-env" \
    CORE_MANIFEST="$core/Cargo.toml" PATH="$bin:$PATH" \
    bash "$TIER" unit -- bash -c 'exit 37' >"$tmp/command.out" 2>"$tmp/command.err"
command_status=$?
check 'wrapped command status is preserved' test "$command_status" -eq 37
check 'copy is transient after command failure' no_new_copies

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
         that ships it (v0.2.13 or later).
EOF
check 'metadata failure is exactly the concise adapter diagnostic' \
    cmp -s "$tmp/metadata.expected" "$tmp/metadata.err"
check 'metadata failure creates no copy' no_new_copies

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
check 'missing wrapper creates no copy' no_new_copies

# A script that IS at the resolved path and is NOT core's wrapper. Resolution
# finding a file there is not the same as finding the right file, and the
# adapter must refuse rather than run it -- "core is broken" reported as "core
# ran fine" is the quietest failure available here.
cat > "$bin/cargo" <<'EOF'
#!/usr/bin/env bash
printf '{"packages":[{"name":"am-fs-core","manifest_path":"%s"}]}\n' "$CORE_MANIFEST"
EOF
impostor="$tmp/core-with-impostor"
mkdir -p "$impostor/scripts"
printf '#!/usr/bin/env bash\necho "some-other-wrapper 9"\n' \
    > "$impostor/scripts/output-budget.sh"
chmod +x "$impostor/scripts/output-budget.sh"
CORE_MANIFEST="$impostor/Cargo.toml" PATH="$bin:$PATH" \
    bash "$TIER" unit -- true >"$tmp/impostor.out" 2>"$tmp/impostor.err"
impostor_status=$?
check "a wrapper that is not rust-fs-core's is refused" test "$impostor_status" -eq 1
check 'the refusal names the version check' grep -q -- '--version' "$tmp/impostor.err"
check 'the refused wrapper creates no copy' no_new_copies

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
check 'partial copy is removed' no_new_copies

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
