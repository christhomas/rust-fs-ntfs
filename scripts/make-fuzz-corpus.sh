#!/usr/bin/env bash
# Rebuild fuzz/corpus from volumes mkntfs wrote.
#
# NTFS is the most structurally complex format in the constellation, and
# several of its parsers run before anything has been validated: the
# boot sector's geometry, the fixup applied to every MFT record, and the
# index blocks a directory lookup walks.
#
# The seeds are volumes the reference tool produced. A random byte
# string is refused by the "NTFS    " OEM name on the first line and
# never reaches the arithmetic underneath.
#
# Usage: scripts/make-fuzz-corpus.sh
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
work="$(mktemp -d "${TMPDIR:-/tmp}/ntfs-fuzz-corpus.XXXXXX")"
trap 'rm -rf "$work"' EXIT

command -v mkntfs >/dev/null || {
    echo "mkntfs not found; install ntfs-3g" >&2
    exit 1
}

rm -rf "$here/fuzz/corpus"
mkdir -p "$here/fuzz/corpus"/{image,boot_sector,mft_record,index_block}

# -F to skip the "this is not a partition" question, -Q for a quick
# format: a full format zeroes the whole volume, which makes a much
# larger file and adds nothing a parser can see.
build() {
    local name="$1" size="$2"; shift 2
    local img="$here/fuzz/corpus/image/$name.img"
    truncate -s "$size" "$img"
    mkntfs -F -Q -L "$name" "$@" "$img" >/dev/null 2>&1 || {
        echo "mkntfs could not build the '$name' volume" >&2
        exit 1
    }
}

# The cluster size is what every run list is measured in, so a
# non-default one changes the arithmetic rather than the layout. 4 KiB
# is the usual; 512 bytes makes the run lists longer and the numbers
# smaller, which is where an off-by-one lives.
build cluster4k 8M  -c 4096
build cluster512 8M -c 512

python3 - "$here/fuzz/corpus" <<'PY'
import os, struct, sys

root = sys.argv[1]
OEM = b'NTFS    '
SECTOR = 512

def write(kind, name, data):
    with open(os.path.join(root, kind, name), 'wb') as f:
        f.write(data)

records = indexes = 0
for img_name in sorted(os.listdir(os.path.join(root, 'image'))):
    stem = img_name[:-len('.img')]
    img = open(os.path.join(root, 'image', img_name), 'rb').read()

    boot = img[:SECTOR]
    assert boot[3:11] == OEM, f"{img_name}: no NTFS OEM name in the boot sector"
    write('boot_sector', f'{stem}.bin', boot)

    bytes_per_sector, = struct.unpack_from('<H', boot, 11)
    sectors_per_cluster = boot[13]
    cluster = bytes_per_sector * sectors_per_cluster
    mft_lcn, = struct.unpack_from('<Q', boot, 0x30)
    clusters_per_record = struct.unpack_from('<b', boot, 0x40)[0]
    # A negative value is a power-of-two byte count, which is the usual
    # case: -10 means 1024-byte records.
    record_size = cluster * clusters_per_record if clusters_per_record > 0 else 1 << (-clusters_per_record)

    mft_at = mft_lcn * cluster
    # The first few MFT records are $MFT, $MFTMirr, $LogFile, $Volume,
    # $AttrDef, ".", $Bitmap, $Boot -- different attribute shapes in
    # each, which is what makes them worth seeding separately.
    for i in range(8):
        at = mft_at + i * record_size
        rec = img[at:at + record_size]
        if rec[:4] != b'FILE':
            continue
        write('mft_record', f'{stem}-rec{i}.bin', rec)
        records += 1

    # Index blocks carry their own signature and are the structure a
    # directory lookup walks.
    for at in range(0, len(img) - record_size + 1, record_size):
        if img[at:at + 4] == b'INDX':
            write('index_block', f'{stem}-at{at // record_size}.bin', img[at:at + record_size])
            indexes += 1
            break

assert records, "no MFT record found -- the boot-sector arithmetic needs revisiting"
assert indexes, "no INDX block found -- did mkntfs leave the root directory resident?"

# `decode_runs`, `decode_eas`, `iter_attributes` and `decompress_unit`
# all take a fragment of an MFT record rather than a whole structure --
# a data-run list and an EA list are attribute *values*, and picking one
# out means parsing the record the fuzzer is about to mutate. They share
# the `mft_record` corpus rather than each keeping a copy of it: four
# identical directories would make the corpus look four times the size
# it is, and would need updating four times. scripts/fuzz-all.sh names
# the mapping.

print(f"{records} MFT records, {indexes} index blocks")
PY

echo "corpus rebuilt under fuzz/corpus:"
find "$here/fuzz/corpus" -type d -mindepth 1 | sort | while read -r d; do
    printf "  %-28s %s files\n" "${d#"$here/"}" "$(ls "$d" | wc -l)"
done
echo "total: $(find "$here/fuzz/corpus" -type f | wc -l) seeds, $(du -sh "$here/fuzz/corpus" | cut -f1)"
