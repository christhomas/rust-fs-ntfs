#!/usr/bin/env bash
# Tests for chkdsk_says in scripts/matrix-fetch-diag.sh: a chkdsk report in,
# the verdict in words out.
#
# It is the line a person reads to know whether Windows found the volume
# clean, so what it must never do is say something reassuring about a report
# it did not understand. Two bugs here were only caught by reading real
# output: ASCII reports decoded as UTF-16 came back as CJK noise, and the
# first fix, `grep $'\x00'`, matched EVERY file because bash turns $'\x00'
# into an empty pattern. Each has a case below that fails on it.
#
# Fixtures are written here, in both encodings chkdsk's report can arrive in:
# ASCII (PowerShell's -RedirectStandardOutput) and UTF-16LE with and without
# a BOM (what Out-File and some consoles write).
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# shellcheck source=../../scripts/matrix-fetch-diag.sh
. "$ROOT/scripts/matrix-fetch-diag.sh"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
pass=0; fail=0

# check NAME ENCODING TEXT WANT -- ENCODING: ascii | utf16 | utf16bom
check() {
    local name="$1" enc="$2" text="$3" want="$4" f got
    f="$tmp/$name.txt"
    case "$enc" in
        ascii)    printf '%s' "$text" > "$f" ;;
        utf16)    printf '%s' "$text" | iconv -f UTF-8 -t UTF-16LE > "$f" ;;
        utf16bom) { printf '\xff\xfe'; printf '%s' "$text" | iconv -f UTF-8 -t UTF-16LE; } > "$f" ;;
    esac
    got="$(chkdsk_says "$f")"
    if [ "$got" = "$want" ]; then
        pass=$((pass + 1)); printf '  ok    %s\n' "$name"
    else
        fail=$((fail + 1)); printf '  FAIL  %s\n        got:  %s\n        want: %s\n' "$name" "$got" "$want"
    fi
}

CLEAN=$'The type of the file system is NTFS.\r\nWindows has scanned the file system and found no problems.\r\nNo further action is required.\r\n     32767 KB total disk space.\r\n'
SNAPSHOT=$'The type of the file system is NTFS.\r\nInsufficient storage available to create either the shadow copy storage file or other shadow copy data.\r\nA snapshot error occured while scanning this drive. Run an offline scan and fix.\r\n'
PROBLEMS=$'The type of the file system is NTFS.\r\nWindows has scanned the file system and found problems.\r\nRun CHKDSK with the /F (fix) option to correct these.\r\n'
REPAIRED=$'The type of the file system is NTFS.\r\nWindows has made corrections to the file system.\r\n'
ODD=$'The type of the file system is NTFS.\r\nSomething chkdsk has never said before.\r\n'

printf 'chkdsk_says\n'
# The report as the matrix actually receives it (the case that came back as noise).
check clean-ascii         ascii    "$CLEAN"    "no problems"
check snapshot-ascii      ascii    "$SNAPSHOT" "NOT SCANNED (snapshot error)"
check problems-ascii      ascii    "$PROBLEMS" "PROBLEMS FOUND"
check repaired-ascii      ascii    "$REPAIRED" "REPAIRED (was not clean)"
# The same reports in UTF-16, with and without a byte-order mark.
check clean-utf16         utf16    "$CLEAN"    "no problems"
check clean-utf16-bom     utf16bom "$CLEAN"    "no problems"
check snapshot-utf16      utf16    "$SNAPSHOT" "NOT SCANNED (snapshot error)"
check problems-utf16-bom  utf16bom "$PROBLEMS" "PROBLEMS FOUND"
# Never reassuring about what it does not understand.
check unrecognised-ascii  ascii    "$ODD"      "unrecognised: Something chkdsk has never said before."
check unrecognised-utf16  utf16    "$ODD"      "unrecognised: Something chkdsk has never said before."
check empty               ascii    ""          "empty report"

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" = 0 ]
