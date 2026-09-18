#!/usr/bin/env bash
# matrix-fetch-diag.sh -- bring the VM's per-scenario diagnostics home, and
# say what they show.
#
# The runner keeps each step's exit code and output under
# test-diagnostics/matrix/<scenario>/, but the Windows ops write their real
# evidence -- chkdsk's report, what Windows enumerated, what a write or
# delete did -- to {vm.workdir}/diag/<scenario>/ ON THE VM (the -Diag
# argument in fs-windows-test-harness.toml). A green step there says only
# "exit 0". This copies that directory to
# test-diagnostics/matrix/<scenario>/vm/ for every scenario of the last run,
# and prints one line per scenario naming what Windows actually reported.
#
# chore matrix runs it as a `defer:`, so it runs when scenarios FAIL too --
# which is when the evidence is wanted.
set -uo pipefail

# chkdsk's verdict, in words, from one chkdsk-<mode>.txt (UTF-16 or ASCII).
chkdsk_says() {
    local f="$1" text
    # PowerShell's redirect writes ASCII; some tools write UTF-16. iconv
    # "succeeds" on ASCII read as UTF-16 and returns noise, so decode as
    # UTF-16 only when the file has the NUL bytes UTF-16 text is full of.
    # With no byte-order mark, iconv's "UTF-16" assumes BIG-endian and turns
    # Windows' little-endian text into CJK noise (tests/scripts caught it),
    # so a BOM decides the order and its absence means LE, as on Windows.
    if [ "$(LC_ALL=C tr -cd '\000' < "$f" | wc -c)" -gt 0 ]; then
        if [ "$(head -c 2 "$f" | od -An -tx1 | tr -d ' \n')" = "fffe" ]; then
            text="$(iconv -f UTF-16 -t UTF-8 "$f" 2>/dev/null)"
        else
            text="$(iconv -f UTF-16LE -t UTF-8 "$f" 2>/dev/null)"
        fi
    else
        text="$(cat "$f")"
    fi
    text="$(printf '%s' "$text" | tr -d '\r')"
    case "$text" in
        *"found no problems"*)                         echo "no problems" ;;
        *"snapshot error"*|*"shadow copy"*)            echo "NOT SCANNED (snapshot error)" ;;
        *"found problems"*|*"Errors found"*|*"errors found"*|*"corrupt"*) echo "PROBLEMS FOUND" ;;
        *"made corrections"*|*"fixed"*)                echo "REPAIRED (was not clean)" ;;
        '')                                            echo "empty report" ;;
        *)                                             echo "unrecognised: $(printf '%s' "$text" | grep -v '^\s*$' | tail -n 1 | cut -c1-60)" ;;
    esac
}

# Everything below touches the VM and the filesystem; the function above does
# not, which is what lets tests/scripts/matrix-fetch-diag.sh source this file
# and test the verdict alone.
main() {
    REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
    DIAG="$REPO/test-diagnostics/matrix"
    cd "$REPO" || exit 1
    # shellcheck disable=SC1091
    [ -f .test-env ] && . ./.test-env
    : "${VM_HOST:?VM_HOST is not set in .test-env}" "${VM_WORKDIR:?VM_WORKDIR is not set in .test-env}"

    key_opts=()
    [ -n "${SSH_KEY:-}" ] && key_opts=(-o IdentitiesOnly=yes -i "$SSH_KEY")
    ssh_opts=(-o BatchMode=yes -o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new "${key_opts[@]+"${key_opts[@]}"}")

    [ -d "$DIAG" ] || { echo "diag: no test-diagnostics/matrix/ -- no matrix run to collect"; exit 0; }


    n=0; failed=0
    : > "$DIAG/summary.txt"
    for dir in "$DIAG"/*/; do
        name="$(basename "$dir")"
        [ -f "$dir/recipe.json" ] || continue
        n=$((n + 1))
        rm -rf "$dir/vm"
        mkdir -p "$dir/vm"
        # scp from Windows OpenSSH: a drive-letter path is addressed as /C:/...
        if ! scp -q -r "${ssh_opts[@]}" "$VM_HOST:/$VM_WORKDIR/diag/$name/." "$dir/vm/" 2>/dev/null; then
            line="$name: no VM diagnostics (nothing ran on the VM, or it is unreachable)"
        else
            parts=()
            for f in "$dir"/vm/chkdsk-*.txt; do
                [ -f "$f" ] || continue
                case "$f" in *-exit.txt) continue ;; esac
                mode="${f##*/chkdsk-}"; mode="${mode%.txt}"
                parts+=("chkdsk $mode: $(chkdsk_says "$f")")
            done
            for f in "$dir"/vm/*-result.txt "$dir"/vm/enumerate.txt; do
                [ -f "$f" ] || continue
                n_lines="$(grep -cv '^\s*$' "$f" 2>/dev/null || echo 0)"
                parts+=("$(basename "$f" .txt): $n_lines line(s)")
            done
            [ "${#parts[@]}" -gt 0 ] || parts=("files: $(ls "$dir/vm" | tr '\n' ' ')")
            line="$name: $(IFS=';'; printf '%s' "${parts[*]}" | sed 's/;/; /g')"
        fi
        case "$line" in *PROBLEMS*|*REPAIRED*|*"NOT SCANNED"*|*unrecognised*|*"no VM diag"*) failed=$((failed + 1)) ;; esac
        printf '%s\n' "$line" >> "$DIAG/summary.txt"
    done

    cat "$DIAG/summary.txt"
    echo "diag: $n scenario(s), $failed with something to look at -- test-diagnostics/matrix/<scenario>/vm/, summary.txt"
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then main "$@"; fi
