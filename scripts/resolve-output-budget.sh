#!/usr/bin/env bash
# Resolve the canonical rust-fs-core output-budget wrapper.
#
# The wrapper itself belongs to rust-fs-core and must not be copied into this
# repository. A coordinated developer checkout uses the sibling directly. A
# standalone checkout uses the packaged Cargo source once rust-fs-core has
# published a version containing the wrapper.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CORE_ROOT="${FS_CORE_ROOT:-$REPO/../rust-fs-core}"
CORE_PACKAGE="am-fs-core"
SCRIPT_REL="scripts/output-budget.sh"
EXPECTED_API="rust-fs-core-output-budget 1"
# This is the digest of the canonical script supplied by rust-fs-core.
EXPECTED_SHA256="5f7fee1f985c1285b04640ae247fcb4ef6e8cc3d63369675610605a2e29c0b99"

die() {
    echo "resolve-output-budget.sh: $*" >&2
    exit 1
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        die "sha256sum or shasum is required to verify $1"
    fi
}

validate() {
    local path="$1" actual version
    [ -f "$path" ] || return 1

    actual="$(sha256 "$path")"
    [ "$actual" = "$EXPECTED_SHA256" ] || return 1

    version="$(bash "$path" --version 2>/dev/null || true)"
    [ "$version" = "$EXPECTED_API" ] || return 1
    printf '%s\n' "$path"
}

# Prefer the sibling so coordinated local core changes are actually tested.
if [ -f "$CORE_ROOT/$SCRIPT_REL" ]; then
    validate "$CORE_ROOT/$SCRIPT_REL" && exit 0
    die "sibling script failed API/checksum validation: $CORE_ROOT/$SCRIPT_REL"
fi

# Cargo metadata is the supported way to locate a resolved dependency. It
# works for a registry package without depending on CARGO_HOME's layout.
metadata="$(mktemp)"
metadata_err="$(mktemp)"
trap 'rm -f "$metadata" "$metadata_err"' EXIT
if cargo metadata --format-version 1 --locked >"$metadata" 2>"$metadata_err"; then
    package_root="$(python3 - "$metadata" "$CORE_PACKAGE" <<'PY'
import json
import pathlib
import sys

metadata_path, package_name = sys.argv[1:]
data = json.loads(pathlib.Path(metadata_path).read_text())
for package in data.get("packages", []):
    if package.get("name") == package_name:
        print(pathlib.Path(package["manifest_path"]).parent)
        break
PY
)"
    if [ -n "$package_root" ]; then
        package_script="$package_root/$SCRIPT_REL"
        validate "$package_script" && exit 0
        die "resolved Cargo package has no verified $SCRIPT_REL: $package_root"
    fi
fi

echo "resolve-output-budget.sh: no sibling or resolved Cargo package supplied the canonical script" >&2
if [ -s "$metadata_err" ]; then
    sed 's/^/  cargo: /' "$metadata_err" >&2
fi
echo "  expected API: $EXPECTED_API" >&2
echo "  expected SHA-256: $EXPECTED_SHA256" >&2
echo "  sibling path: $CORE_ROOT/$SCRIPT_REL" >&2
echo "  install a rust-fs-core release containing $SCRIPT_REL, or run 'chore siblings'." >&2
exit 1
