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
#
# EVERY STEP HERE IS BYTE-ORIENTED, hence LC_ALL=C throughout. chkdsk prints
# the volume label, and a label like "Disk eclipse" with an e-acute arrives as
# one 0xE9 byte -- Windows' ANSI codepage, not UTF-8. In a UTF-8 locale that
# byte is an illegal sequence: `tr` fails outright ("tr: Illegal byte
# sequence") and the text it should have cleaned is lost, so a volume chkdsk
# called clean was reported as `unrecognised: Volume label is Disk ` --
# truncated at the very byte that broke it (mac-format-label-latin1, every run
# until 2026-09-18). Reading bytes as bytes cannot fail this way; the cost is
# that the quoted tail of an unrecognised report may cut a multi-byte
# character in half, which is cosmetic.
chkdsk_says() {
    local f="$1" text
    # -x, because `local LC_ALL=C` alone would set a shell variable the
    # commands below never see: they need it in their environment.
    local -x LC_ALL=C
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

# Compare the structured chkdsk record with the verdict shape that produced
# it. stdout is one of `match`, `mismatch: ...`, `not-scanned: ...`, or
# `unknown: ...`; only `match` is quiet. The mode states are the contract from
# fs-windows-test-harness#29. In particular, an offline fallback may make the
# step pass, but it must not hide that the requested online scan did not run.
verdict_says() {
    python3 - "$1" <<'PY'
import json
import sys
from pathlib import Path

path = Path(sys.argv[1])
if not path.is_file():
    print("unknown: missing verdict.json")
    raise SystemExit
try:
    verdict = json.loads(path.read_text(encoding="utf-8-sig"))
except (OSError, UnicodeError, json.JSONDecodeError):
    print("unknown: unreadable verdict.json")
    raise SystemExit
if not isinstance(verdict, dict):
    print("unknown: verdict.json is not an object")
    raise SystemExit

shape = verdict.get("verdict_shape")
modes = verdict.get("modes")
if not isinstance(modes, dict) or not modes:
    print("unknown: verdict.json has no per-mode states")
    raise SystemExit

states = {}
for mode, result in modes.items():
    if not isinstance(mode, str) or not mode:
        print("unknown: verdict.json has an invalid mode name")
        raise SystemExit
    if not isinstance(result, dict) or result.get("state") not in {
        "scanned", "not-scanned", "failed"
    }:
        print(f"unknown: {mode} has no recognised state")
        raise SystemExit
    if (not isinstance(result.get("exit"), int)
            or isinstance(result.get("exit"), bool)
            or not isinstance(result.get("reason"), str)
            or not result.get("reason")):
        print(f"unknown: {mode} has malformed exit or reason")
        raise SystemExit
    states[mode] = result["state"]

not_scanned = [mode for mode, state in states.items() if state == "not-scanned"]
if not_scanned:
    print("not-scanned: " + ", ".join(sorted(not_scanned)))
    raise SystemExit
if verdict.get("passed") is not True:
    print("mismatch: verdict did not pass")
    raise SystemExit

if shape == "clean":
    wrong = [f"{mode}={state}" for mode, state in states.items() if state != "scanned"]
elif shape == "repair-required":
    required = {"/scan": "failed", "/F /X": "scanned", "/scan-post": "scanned"}
    wrong = [
        f"{mode}={states.get(mode, 'missing')} (expected {expected})"
        for mode, expected in required.items()
        if states.get(mode) != expected
    ]
    wrong += [
        f"{mode}={state} (expected scanned)"
        for mode, state in states.items()
        if mode not in required and state != "scanned"
    ]
else:
    print(f"unknown: unsupported verdict shape {shape!r}")
    raise SystemExit

print("mismatch: " + ", ".join(wrong) if wrong else "match")
PY
}

# Is this summary line one a person needs to read? The count of them is the
# run's headline, and they are the only lines printed -- so a verdict this
# does not recognise as right is a verdict nobody sees. Structured verdicts
# decide whether PROBLEMS/REPAIRED are expected for this recipe; text parsing
# remains the escape hatch for reports the structured record cannot explain.
looks_wrong() {
    local line="$1" verdict="${2:-none}"
    case "$line" in
        *"NOT SCANNED"*|*unrecognised*|*"no VM diag"*|*"empty report"*) return 0 ;;
    esac
    case "$verdict" in
        match) return 1 ;;
        mismatch:*|not-scanned:*|unknown:*) return 0 ;;
    esac
    case "$line" in
        *PROBLEMS*|*REPAIRED*) return 0 ;;
        *) return 1 ;;
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
    # The agent that holds the VM's key. SSH_KEY is a .pub: the private half is
    # served by trove, and a shell whose SSH_AUTH_SOCK points somewhere else
    # (launchd's agent, on macOS) offers no key and every ssh here fails as if
    # the VM were at fault. See scripts/vm-ssh-agent.sh.
    SSH_AUTH_SOCK="$("$REPO/scripts/vm-ssh-agent.sh")"; export SSH_AUTH_SOCK
    : "${VM_HOST:?VM_HOST is not set in .test-env}" "${VM_WORKDIR:?VM_WORKDIR is not set in .test-env}"

    key_opts=()
    [ -n "${SSH_KEY:-}" ] && key_opts=(-o IdentitiesOnly=yes -i "$SSH_KEY")
    ssh_opts=(-o BatchMode=yes -o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new "${key_opts[@]+"${key_opts[@]}"}")

    [ -d "$DIAG" ] || { echo "diag: no test-diagnostics/matrix/ -- no matrix run to collect"; exit 0; }


    n=0; failed=0; attention=()
    : > "$DIAG/summary.txt"
    for dir in "$DIAG"/*/; do
        name="$(basename "$dir")"
        [ -f "$dir/recipe.json" ] || continue
        n=$((n + 1))
        rm -rf "$dir/vm"
        mkdir -p "$dir/vm"
        verdict_status=none
        # scp from Windows OpenSSH: a drive-letter path is addressed as /C:/...
        if ! scp -q -r "${ssh_opts[@]}" "$VM_HOST:/$VM_WORKDIR/diag/$name/." "$dir/vm/" 2>/dev/null; then
            # A MAC-ONLY SCENARIO HAS NOTHING THERE, and saying so as though
            # it were a problem costs the reader four lines a run and teaches
            # them to skim the ones that are. The recipe says which it is:
            # every step that runs on the VM is recorded with "host": "vm".
            if grep -q '"host": *"vm"' "$dir/recipe.json" 2>/dev/null; then
                line="$name: no VM diagnostics (nothing ran on the VM, or it is unreachable)"
            else
                line="$name: no VM steps (host-only scenario)"
            fi
        else
            parts=()
            has_chkdsk=0
            for f in "$dir"/vm/chkdsk-*.txt; do
                [ -f "$f" ] || continue
                case "$f" in *-exit.txt) continue ;; esac
                has_chkdsk=1
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
            if [ "$has_chkdsk" = 1 ]; then
                verdict_status="$(verdict_says "$dir/vm/verdict.json")"
                [ "$verdict_status" = match ] || line="$line; verdict: $verdict_status"
            fi
        fi
        if looks_wrong "$line" "$verdict_status"; then
            failed=$((failed + 1))
            attention+=("$line")
        fi
        printf '%s\n' "$line" >> "$DIAG/summary.txt"
    done

    # QUIET WHEN THERE IS NOTHING TO SAY. Printing a line per scenario means
    # 46 lines after a run where every one of them says "no problems", and
    # the reader who pays for that is the one who re-reads this transcript on
    # every later step. The lines worth reading are printed; summary.txt
    # holds all of them either way.
    [ "${#attention[@]}" = 0 ] || printf '%s\n' "${attention[@]}"
    echo "diag: $n scenario(s), $failed with something to look at -- test-diagnostics/matrix/<scenario>/vm/, summary.txt"
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then main "$@"; fi
