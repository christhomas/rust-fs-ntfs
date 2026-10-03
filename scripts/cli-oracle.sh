#!/usr/bin/env bash
# cli-oracle.sh STEP IMAGE [ARG...] -- one host-side step of a Windows
# oracle scenario for the command-line tools (the `cli-*` ops in
# fs-windows-test-harness.toml; the scenarios are the `cli` group in
# test-matrix.json).
#
# The tools make and change the image here; Windows then grades it
# (win-chkdsk, win-dirty-query, win-cli-verify). Each step runs the
# multi-call binary under its repository name -- `rust-fs-ntfs mkfs ...` is
# the same program as `mkfs.ntfs ...` and needs no links, which a Windows
# checkout does not have -- and checks what the tool itself reported, so a
# scenario fails at the step that went wrong rather than at chkdsk.
#
# STEPS
#   mkfs IMAGE SIZE_MIB LABEL        mkfs.ntfs --size, exit 0, the label read back
#   set-dirty IMAGE                  fs.ntfs set dirty true
#   corrupt IMAGE mirror|signature   damage the volume by its on-disk layout
#   fsck-finds IMAGE KIND            fsck.ntfs exits 4 and reports a finding of KIND
#   populate IMAGE LABEL             fs.ntfs mkdir, write (every size that changes
#                                    how NTFS stores a file, replacements shorter
#                                    and longer) and set label; IMAGE.manifest then
#                                    lists every file's SHA-256, every directory and
#                                    the label, for win-cli-verify.ps1
#   replay IMAGE SNAPSHOT            IMAGE is test-disks/windows-interrupted-SNAPSHOT,
#                                    whose log holds work; fsck.ntfs reports it,
#                                    fsck.ntfs -y replays it, and IMAGE.manifest is
#                                    what Windows listed after recovering the same
#                                    image itself, for win-cli-verify.ps1
#   mount-replay IMAGE SNAPSHOT      IMAGE is test-disks/windows-interrupted-SNAPSHOT;
#                                    fsck.ntfs reports its log, then fs.ntfs mkdir and
#                                    write, which replay the log at mount as Windows
#                                    does, and fsck.ntfs after finds nothing.
#                                    IMAGE.manifest is Windows' listing of its own
#                                    recovery plus what fs.ntfs wrote
#   grow-mft IMAGE SNAPSHOT          IMAGE is what Windows recovered from
#                                    test-disks/windows-interrupted-SNAPSHOT, whose
#                                    $MFT:$Bitmap outruns $MFT; fs.ntfs writes files
#                                    until $MFT has grown twice, and fsck.ntfs after
#                                    finds nothing. IMAGE.manifest is Windows' listing
#                                    plus every file fs.ntfs wrote
#
# The binary is target/release/rust-fs-ntfs (built with `--features cli`),
# or RUST_FS_NTFS. JSON is read with grep, not jq: the Windows runner's Git
# Bash is not promised to carry jq, and each check is one field.
set -euo pipefail

# Git Bash rewrites any argument that looks like a POSIX path before it
# reaches a native program, so `fs.ntfs IMAGE mkdir /d` arrived as `D:/`.
# The paths passed to the tool are paths inside the NTFS image, never on
# the host: nothing here wants the rewrite.
export MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*'

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${RUST_FS_NTFS:-$REPO/target/release/rust-fs-ntfs}"

die() {
    echo "cli-oracle: $*" >&2
    exit 1
}

[ $# -ge 2 ] || die "usage: cli-oracle.sh STEP IMAGE [ARG...]"
step="$1"
image="$2"
shift 2

[ -x "$BIN" ] || [ -x "$BIN.exe" ] ||
    die "no $BIN: build it with \`cargo build --release --locked --features cli --bin rust-fs-ntfs\`"

# run TOOL ARG...: the tool's report in $out, its status in $status. Quiet:
# a failing step prints the report it is failing on, a passing one a line.
run() {
    set +e
    out="$("$BIN" "$@" 2>"$image.cli-err")"
    status=$?
    set -e
}

# has PATTERN: the report carries PATTERN (a `"key": value` line).
has() { grep -q -- "$1" <<<"$out"; }

# The on-disk layout, read from the boot sector (MS-FSCC), not from the
# code under test: where the damage goes must not be the tool's answer.
u8() { od -An -tu1 -j "$2" -N 1 "$1" | tr -d ' \n\r'; }
u16() { od -An -tu2 -j "$2" -N 2 "$1" | tr -d ' \n\r'; }
u64() { od -An -tu8 -j "$2" -N 8 "$1" | tr -d ' \n\r'; }
cluster_size() { echo $(($(u16 "$1" 11) * $(u8 "$1" 13))); }
record_size() {
    local n
    n="$(u8 "$1" 64)"
    if [ "$n" -lt 128 ]; then echo $((n * $(cluster_size "$1"))); else echo $((1 << (256 - n))); fi
}
poke() { printf "$3" | dd of="$1" bs=1 seek="$2" conv=notrunc 2>/dev/null; }
sha() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1; }
random() { if [ "$2" -gt 0 ]; then head -c "$2" /dev/urandom >"$1"; else : >"$1"; fi; }

# windows_listing SNAPSHOT: what Windows listed after recovering the same
# image itself -- path, size and Get-FileHash SHA-256 -- as win-cli-verify
# reads it, to IMAGE.manifest.
windows_listing() {
    awk -F '\t' 'NF == 3 { printf "file\t/%s\t%s\t%s\n", $1, $2, $3 }' \
        "$REPO/test-disks/windows-interrupted-$1.recovered.manifest" >"$image.manifest"
    [ -s "$image.manifest" ] || die "no manifest for snapshot $1"
}

# unpack_interrupted SNAPSHOT: IMAGE is the volume Windows left mid-write,
# and fsck.ntfs must see its log holding work.
unpack_interrupted() {
    mkdir -p "$(dirname "$image")"
    gzip -dc "$REPO/test-disks/windows-interrupted-$1.img.gz" >"$image" ||
        die "no test-disks/windows-interrupted-$1.img.gz"
    run fsck "$image"
    [ "$status" -eq 4 ] || die "fsck.ntfs on the volume Windows left mid-write exited $status, not 4: $out"
    has '"kind": "logfile"' || die "fsck.ntfs did not report the log holding work: $out"
}

case "$step" in
    mkfs)
        [ $# -eq 2 ] || die "mkfs IMAGE SIZE_MIB LABEL"
        mkdir -p "$(dirname "$image")"
        rm -f "$image"
        run mkfs --size "$1M" --label "$2" "$image"
        [ "$status" -eq 0 ] || die "mkfs.ntfs exited $status: $(cat "$image.cli-err")"
        has "\"label\": \"$2\"" || die "mkfs.ntfs's report does not read back the label $2"
        has '"formatted": true' || die "mkfs.ntfs's report does not say it formatted"
        ;;
    set-dirty)
        run fs "$image" set dirty true
        [ "$status" -eq 0 ] || die "fs.ntfs set dirty true exited $status: $(cat "$image.cli-err")"
        run fs "$image" get dirty --text
        [ "$out" = true ] || die "get dirty after set dirty true says '$out'"
        ;;
    corrupt)
        [ $# -eq 1 ] || die "corrupt IMAGE mirror|signature"
        cluster="$(cluster_size "$image")"
        record="$(record_size "$image")"
        case "$1" in
            # One byte of $MFTMirr's copy of record 0 ($MFT), well inside
            # the record: the mirror (+0x38) no longer agrees with $MFT.
            mirror) poke "$image" $(($(u64 "$image" 56) * cluster + 200)) '\x5a' ;;
            # The FILE signature of record 13, a reserved record mkfs.ntfs
            # marks in use: $MFT (+0x30) is one run on a fresh volume.
            signature) poke "$image" $(($(u64 "$image" 48) * cluster + 13 * record)) 'XXXX' ;;
            *) die "corrupt: '$1' is not mirror or signature" ;;
        esac
        ;;
    fsck-finds)
        [ $# -eq 1 ] || die "fsck-finds IMAGE KIND"
        run fsck "$image"
        [ "$status" -eq 4 ] || die "fsck.ntfs exited $status, not 4 (errors left): $out"
        has "\"kind\": \"$1\"" || die "fsck.ntfs reported no $1 finding: $out"
        ;;
    populate)
        [ $# -eq 1 ] || die "populate IMAGE LABEL"
        manifest="$image.manifest"
        work="$(mktemp -d)"
        trap 'rm -rf "$work"' EXIT
        : >"$manifest"
        for dir in /d /d/e; do
            run fs "$image" mkdir "$dir"
            [ "$status" -eq 0 ] || die "fs.ntfs mkdir $dir exited $status: $(cat "$image.cli-err")"
            printf 'dir\t%s\n' "$dir" >>"$manifest"
        done
        # path:size, then path:size again for a replacement.
        for spec in /empty.bin:0 /one.bin:1 /resident.bin:500 /cluster.bin:4096 \
            /cluster-plus-one.bin:4097 /random.bin:1048576 /d/e/deep.bin:5000 \
            "/na$(printf '\303\257')ve-$(printf '\346\227\245\346\234\254').txt:42" \
            /shrunk.bin:4097 /shrunk.bin:1 /grown.bin:1 /grown.bin:1048576; do
            path="${spec%:*}"
            size="${spec##*:}"
            random "$work/data" "$size"
            set +e
            "$BIN" fs "$image" write "$path" <"$work/data" >/dev/null 2>"$image.cli-err"
            status=$?
            set -e
            [ "$status" -eq 0 ] || die "fs.ntfs write $path ($size bytes) exited $status: $(cat "$image.cli-err")"
            grep -v "^file	$path	" "$manifest" >"$work/m" || true
            printf 'file\t%s\t%s\t%s\n' "$path" "$size" "$(sha "$work/data")" >>"$work/m"
            cp "$work/m" "$manifest"
        done
        run fs "$image" set label "$1"
        [ "$status" -eq 0 ] || die "fs.ntfs set label exited $status: $(cat "$image.cli-err")"
        printf 'label\t%s\n' "$1" >>"$manifest"
        run fsck "$image"
        [ "$status" -eq 0 ] || die "fsck.ntfs after the writes exited $status: $out"
        ;;
    replay)
        [ $# -eq 1 ] || die "replay IMAGE SNAPSHOT"
        unpack_interrupted "$1"
        run fsck -y "$image"
        [ "$status" -eq 1 ] || die "fsck.ntfs -y exited $status, not 1 (corrected): $out"
        has '"logfile": "empty"' || die "fsck.ntfs -y did not leave the log empty: $out"
        run fsck "$image"
        [ "$status" -eq 0 ] || die "fsck.ntfs after the replay exited $status: $out"
        windows_listing "$1"
        ;;
    mount-replay)
        [ $# -eq 1 ] || die "mount-replay IMAGE SNAPSHOT"
        unpack_interrupted "$1"
        windows_listing "$1"
        work="$(mktemp -d)"
        trap 'rm -rf "$work"' EXIT
        # No fsck.ntfs -y: the first write's mount replays the log, as
        # Windows does when it mounts the volume. Then a directory, a
        # non-resident file in it, a resident file in a directory the log
        # changed, and a file the log created, replaced. Growing $MFT on
        # the same volume is the grow-mft step's.
        replaced="$(awk -F '\t' '$2 ~ /^\/d000\// { print $2; exit }' "$image.manifest")"
        [ -n "$replaced" ] || die "Windows' listing has no file under /d000"
        run fs "$image" mkdir /after-replay
        [ "$status" -eq 0 ] || die "fs.ntfs mkdir over a log holding work exited $status: $(cat "$image.cli-err")"
        printf 'dir\t/after-replay\n' >>"$image.manifest"
        for spec in /after-replay/new.bin:5000 /d000/after-replay.txt:500 "$replaced:4097"; do
            path="${spec%:*}"
            size="${spec##*:}"
            random "$work/data" "$size"
            set +e
            "$BIN" fs "$image" write "$path" <"$work/data" >/dev/null 2>"$image.cli-err"
            status=$?
            set -e
            [ "$status" -eq 0 ] || die "fs.ntfs write $path ($size bytes) exited $status: $(cat "$image.cli-err")"
            grep -v "^file	$path	" "$image.manifest" >"$work/m" || true
            printf 'file\t%s\t%s\t%s\n' "$path" "$size" "$(sha "$work/data")" >>"$work/m"
            cp "$work/m" "$image.manifest"
        done
        run fsck "$image"
        [ "$status" -eq 0 ] || die "fsck.ntfs after the writes exited $status: $out"
        has '"logfile": "empty"' || die "the mount did not leave the log empty: $out"
        ;;
    grow-mft)
        [ $# -eq 1 ] || die "grow-mft IMAGE SNAPSHOT"
        mkdir -p "$(dirname "$image")"
        gzip -dc "$REPO/test-disks/windows-interrupted-$1.recovered.img.gz" >"$image" ||
            die "no test-disks/windows-interrupted-$1.recovered.img.gz"
        windows_listing "$1"
        work="$(mktemp -d)"
        trap 'rm -rf "$work"' EXIT
        # How many records $MFT holds, as fs.ntfs info reports it. Only a
        # loop bound: Windows grades what was written, and the crate's own
        # test reads the count from $MFT's run list.
        records() {
            run fs "$image" info
            [ "$status" -eq 0 ] || die "fs.ntfs info exited $status: $(cat "$image.cli-err")"
            sed -n 's/.*"mft_total_records": \([0-9]*\).*/\1/p' <<<"$out"
        }
        start="$(records)"
        [ -n "$start" ] || die "fs.ntfs info reported no mft_total_records: $out"
        run fs "$image" mkdir /grown
        [ "$status" -eq 0 ] || die "fs.ntfs mkdir /grown exited $status: $(cat "$image.cli-err")"
        printf 'dir\t/grown\n' >>"$image.manifest"
        # Every record $MFT holds is used, then two growths of 64 more.
        # Each 32-file batch checks how far $MFT has come; the cap is far
        # past what that needs, so a $MFT that never grows fails here.
        i=0
        while [ "$(records)" -le $((start + 64)) ]; do
            [ "$i" -lt 2048 ] || die "$i files written and \$MFT still holds $(records) records, $start at the start"
            for _ in $(seq 32); do
                path="$(printf '/grown/f%04d.bin' "$i")"
                size=$((300 + i))
                random "$work/data" "$size"
                set +e
                "$BIN" fs "$image" write "$path" <"$work/data" >/dev/null 2>"$image.cli-err"
                status=$?
                set -e
                [ "$status" -eq 0 ] || die "fs.ntfs write $path ($size bytes, file $i) exited $status: $(cat "$image.cli-err")"
                printf 'file\t%s\t%s\t%s\n' "$path" "$size" "$(sha "$work/data")" >>"$image.manifest"
                i=$((i + 1))
            done
        done
        run fsck "$image"
        [ "$status" -eq 0 ] || die "fsck.ntfs after the writes exited $status: $out"
        ;;
    *)
        die "unknown step '$step'"
        ;;
esac
rm -f "$image.cli-err"
echo "cli-oracle: $step $image: ok"
