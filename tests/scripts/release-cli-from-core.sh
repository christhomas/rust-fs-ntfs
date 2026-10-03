#!/usr/bin/env bash
# The release tarball is packaged, attested and attached by rust-fs-core's
# reusable release-cli workflow, and this repository keeps no copy (#428).
#
# Ten repositories each carried a `package-cli` matrix, an attest-and-attach
# job and a scripts/package-cli.sh, and the copies drifted: one release never
# got the jobs and shipped no tarball at all (rust-fs-core#193). The one copy
# is now core's `.github/workflows/release-cli.yml`. This holds release.yml
# to calling it:
#
#   - by commit SHA, never a tag or branch, because a called workflow runs
#     with this repository's write grants and a moved ref would change what
#     runs with them;
#   - with `core-ref` equal to chores.yml's FS_CORE_REF, so the release
#     packages with the same core scripts/core.sh was copied from and the
#     pull-request `cli` job packaged with;
#   - with `toolchain` equal to rust-toolchain.toml's channel;
#   - granting exactly what its attach job needs, and gated on the same two
#     jobs crates.io publishing is;
#
# and holds the repository to having no copy left: no scripts/package-cli.sh,
# no test of one, no attest step or package-cli job of its own, nothing that
# runs a local copy, and the [package.metadata.package-cli] table core's
# script reads what ships from.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
pass=0
fail=0

ok()  { pass=$((pass + 1)); }
bad() { fail=$((fail + 1)); printf 'FAIL %s\n' "$*"; }

SHARED='antimatter-studios/rust-fs-core/.github/workflows/release-cli.yml'

# Prints the body of the job in workflow $1 that `uses:` the shared workflow,
# each line as written, or nothing when no job does.
caller_job() {
    awk -v shared="$SHARED" '
        /^jobs:$/ { in_jobs = 1; next }
        in_jobs && /^[^[:space:]#]/ { in_jobs = 0 }
        in_jobs && /^  [A-Za-z0-9_-]+:[[:space:]]*$/ {
            if (found) exit
            body = $0 "\n"; next
        }
        in_jobs && body != "" {
            body = body $0 "\n"
            if ($0 ~ "^    uses:[[:space:]]*" shared "@") found = 1
        }
        END { if (found) printf "%s", body }
    ' "$1"
}

# Every complaint about release.yml's call, one per line; nothing when sound.
call_complaints() {
    local wf="$1" chores="$2" toolchain_file="$3" job uses sha core_ref toolchain
    job="$(caller_job "$wf")"
    if [ -z "$job" ]; then
        echo "no job calls $SHARED"
        return
    fi
    uses="$(printf '%s\n' "$job" | sed -n "s|^    uses:[[:space:]]*$SHARED@\([^[:space:]]*\).*|\1|p")"
    sha="${uses%%[[:space:]#]*}"
    [[ "$sha" =~ ^[0-9a-f]{40}$ ]] || echo "the call is pinned to '$sha', not a commit SHA"

    core_ref="$(printf '%s\n' "$job" | sed -n 's/^      core-ref:[[:space:]]*"\{0,1\}\([^"[:space:]]*\)"\{0,1\}.*/\1/p')"
    want_core="$(sed -n 's/^  FS_CORE_REF:[[:space:]]*\([^[:space:]]*\).*/\1/p' "$chores")"
    [ -n "$core_ref" ] && [ "$core_ref" = "$want_core" ] \
        || echo "core-ref is '$core_ref', but chores.yml's FS_CORE_REF is '$want_core'"

    toolchain="$(printf '%s\n' "$job" | sed -n 's/^      toolchain:[[:space:]]*"\{0,1\}\([^"[:space:]]*\)"\{0,1\}.*/\1/p')"
    want_toolchain="$(sed -n 's/^channel[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$toolchain_file")"
    [ -n "$toolchain" ] && [ "$toolchain" = "$want_toolchain" ] \
        || echo "toolchain is '$toolchain', but rust-toolchain.toml's channel is '$want_toolchain'"

    for grant in 'contents: write' 'id-token: write' 'attestations: write'; do
        printf '%s\n' "$job" | grep -qE "^      $grant[[:space:]]*$" \
            || echo "the calling job does not grant '$grant', which the attach job needs"
    done

    needs="$(printf '%s\n' "$job" | sed -n 's/^    needs:[[:space:]]*//p')"
    for gate in crates-test validate-windows; do
        [[ "$needs" =~ (^|[^a-z-])$gate([^a-z-]|$) ]] \
            || echo "the calling job does not need $gate, so an unvalidated binary could ship"
    done
}

# Every copy of core's packaging still in tree $1, one per line.
copy_complaints() {
    local root="$1" path hits
    for path in scripts/package-cli.sh tests/scripts/package-cli.sh; do
        [ -e "$root/$path" ] && echo "$path is a copy; core's scripts/package-cli.sh is the only one"
    done
    grep -qE '^  package-cli:[[:space:]]*$' "$root/.github/workflows/release.yml" \
        && echo "release.yml still has a package-cli job of its own"
    grep -qE '^[[:space:]]*-?[[:space:]]*uses:[[:space:]]*actions/attest-build-provenance' "$root/.github/workflows/release.yml" \
        && echo "release.yml still attests the tarballs itself"
    hits="$(grep -nE 'scripts/package-cli\.sh' "$root"/.github/workflows/*.yml "$root/chores.yml" "$root"/scripts/*.sh 2>/dev/null \
        | grep -vE ':[0-9]+:[[:space:]]*#' || true)"
    [ -n "$hits" ] && echo "these run a local copy instead of scripts/core.sh package-cli: ${hits//$root\//}"
    grep -qE '^\[package\.metadata\.package-cli\]' "$root/Cargo.toml" \
        || echo "Cargo.toml has no [package.metadata.package-cli]; core's script reads what ships from it"
    grep -qE '\|package-cli[|)]' "$root/scripts/core.sh" \
        || echo "scripts/core.sh does not run package-cli; copy core's again"
}

# The repository as it is.
complaints="$(call_complaints "$ROOT/.github/workflows/release.yml" "$ROOT/chores.yml" "$ROOT/rust-toolchain.toml")"
if [ -z "$complaints" ]; then ok; else bad "release.yml's call to the shared workflow:"$'\n'"$complaints"; fi
complaints="$(copy_complaints "$ROOT")"
if [ -z "$complaints" ]; then ok; else bad "a copy of core's packaging remains:"$'\n'"$complaints"; fi

# The checks must fail on the cases they exist for.
sandbox="$(mktemp -d)"
trap 'rm -rf "$sandbox"' EXIT
printf '  FS_CORE_REF: v0.2.23\n' > "$sandbox/chores.yml"
printf '[toolchain]\nchannel = "1.95.0"\n' > "$sandbox/rust-toolchain.toml"
good_call() {
    cat <<EOF
jobs:
  crates-test:
    runs-on: ubuntu-latest
  cli:
    needs: [crates-test, validate-windows]
    permissions:
      contents: write
      id-token: write
      attestations: write
    uses: $SHARED@${1:-0f7714910080f5f2a080ed6032cccf19bc338064} # v0.2.23
    with:
      core-ref: ${2:-v0.2.23}
      toolchain: ${3:-1.95.0}
EOF
}
expect() { # expect WORKFLOW-TEXT WANT-SUBSTRING-OR-EMPTY WHAT
    printf '%s\n' "$1" > "$sandbox/release.yml"
    local got
    got="$(call_complaints "$sandbox/release.yml" "$sandbox/chores.yml" "$sandbox/rust-toolchain.toml")"
    if [ -z "$2" ]; then
        if [ -z "$got" ]; then ok; else bad "$3: complained: $got"; fi
    else
        case "$got" in *"$2"*) ok ;; *) bad "$3: wanted a complaint naming '$2', got '$got'" ;; esac
    fi
}
expect "$(good_call)" "" "a sound call"
expect "$(good_call v0.2.23)" "not a commit SHA" "a call pinned to a tag"
expect "$(good_call 0f7714910080f5f2a080ed6032cccf19bc338064 v0.2.18)" "FS_CORE_REF" "a core-ref chores.yml does not pin"
expect "$(good_call 0f7714910080f5f2a080ed6032cccf19bc338064 v0.2.23 stable)" "rust-toolchain.toml" "a floating toolchain"
expect "$(good_call | grep -v 'attestations: write')" "attestations: write" "a call without the attestation grant"
expect "$(good_call | sed 's/needs: \[crates-test, validate-windows\]/needs: [crates-test]/')" "validate-windows" "a call not gated on chkdsk"
expect "$(good_call | grep -v 'uses:')" "no job calls" "no call at all"

mkdir -p "$sandbox/tree/scripts" "$sandbox/tree/.github/workflows" "$sandbox/tree/tests/scripts"
cp "$ROOT/scripts/core.sh" "$sandbox/tree/scripts/core.sh"
printf '[package.metadata.package-cli]\n' > "$sandbox/tree/Cargo.toml"
printf 'tasks: {}\n' > "$sandbox/tree/chores.yml"
cat > "$sandbox/tree/.github/workflows/release.yml" <<'EOF'
jobs:
  package-cli:
    steps:
      - run: scripts/package-cli.sh 1.0.0 linux-x86_64
      - uses: actions/attest-build-provenance@v4
EOF
: > "$sandbox/tree/scripts/package-cli.sh"
got="$(copy_complaints "$sandbox/tree")"
for want in 'scripts/package-cli.sh is a copy' 'package-cli job of its own' 'attests the tarballs itself' 'run a local copy'; do
    case "$got" in *"$want"*) ok ;; *) bad "a tree with a packaging copy: wanted '$want', got '$got'" ;; esac
done

if [ "$fail" -gt 0 ]; then
    echo "release-cli-from-core: $pass passed, $fail failed"
    exit 1
fi
echo "release-cli-from-core: $pass checks passed"
