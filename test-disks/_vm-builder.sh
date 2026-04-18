#!/bin/sh
# GUEST-side: runs inside the qemu Alpine VM. Expects the local.d wrapper
# to have: mounted /host via 9p, extracted ntfs-3g + fuse2 apks in-place,
# and ensured /dev/fuse exists. mkntfs, ntfs-3g, setfattr, busybox
# dd/truncate are therefore on PATH.
#
# Each build_* function produces ONE ntfs-*.img in /host so it lands
# directly on the host filesystem. ntfs-3g mount is used ONLY here to
# populate fixtures with known content — the fs-ntfs test binary reads
# these images raw, never through ntfs-3g.

set -eu
cd /host

MNT=/mnt/img
mkdir -p "$MNT"

mount_ntfs() {
    # streams_interface=windows lets us write alternate data streams via
    # path:stream syntax. big_writes improves throughput for large-file
    # fixtures.
    ntfs-3g -o streams_interface=windows,big_writes,default_permissions \
        "$1" "$MNT"
}

umount_ntfs() {
    sync
    umount "$MNT"
    # ntfs-3g unmount is async; wait for the FUSE handle to drop before
    # touching the image again.
    while pgrep -x ntfs-3g >/dev/null 2>&1; do
        sleep 0.1
    done
}

# --- image builders -------------------------------------------------------

build_basic() {
    local img=ntfs-basic.img
    echo "[vm] $img"
    rm -f $img
    truncate -s 16M $img
    mkntfs -q -F -f -L "BasicNTFS" -c 4096 --with-uuid $img
    mount_ntfs $img
    printf 'Hello from NTFS!\n' > $MNT/hello.txt
    mkdir -p $MNT/Documents
    printf 'Test document content\n' > $MNT/Documents/readme.txt
    printf 'Some notes here.\n'       > $MNT/Documents/notes.txt
    umount_ntfs
}

build_manyfiles() {
    local img=ntfs-manyfiles.img
    echo "[vm] $img"
    rm -f $img
    truncate -s 32M $img
    mkntfs -q -F -f -L "ManyFiles" -c 4096 --with-uuid $img
    mount_ntfs $img
    mkdir -p $MNT/bigdir
    i=1
    while [ $i -le 512 ]; do
        printf 'content of file %03d\n' $i > $MNT/bigdir/file_$i.txt
        i=$((i + 1))
    done
    printf 'control\n' > $MNT/small.txt
    umount_ntfs
}

build_large_file() {
    local img=ntfs-large-file.img
    echo "[vm] $img"
    rm -f $img
    truncate -s 64M $img
    mkntfs -q -F -f -L "LargeFile" -c 4096 --with-uuid $img
    mount_ntfs $img
    # 8 MB of zeros, then stamp a distinct ASCII byte ('A'..'H') at the
    # start of each 1 MB block. Tests read those marker bytes to verify
    # offset-correct reads across a non-resident $DATA attribute.
    dd if=/dev/zero of=$MNT/big.bin bs=1M count=8 status=none
    i=0
    while [ $i -lt 8 ]; do
        off=$((i * 1048576))
        byte=$(printf '\\%03o' $((0101 + i)))
        printf "$byte" | dd of=$MNT/big.bin bs=1 count=1 seek=$off conv=notrunc status=none
        i=$((i + 1))
    done
    printf 'small control file\n' > $MNT/small.txt
    umount_ntfs
}

build_sparse() {
    local img=ntfs-sparse.img
    echo "[vm] $img"
    rm -f $img
    truncate -s 32M $img
    mkntfs -q -F -f -L "Sparse" -c 4096 --with-uuid $img
    mount_ntfs $img
    # Create a 4 MB file with holes: write 1 byte at offset 0, 1MB, 2MB, 3MB
    # and truncate to 4 MB. ntfs-3g will set the NTFS sparse flag when
    # unwritten ranges would produce compressed/sparse clusters — we also
    # flip the sparse attribute bit explicitly via setfattr for determinism.
    dd if=/dev/zero of=$MNT/sparse.bin bs=1 count=0 seek=4M status=none
    off=0
    while [ $off -lt 4000000 ]; do
        printf 'X' | dd of=$MNT/sparse.bin bs=1 count=1 seek=$off conv=notrunc status=none
        off=$((off + 1048576))
    done
    printf 'dense\n' > $MNT/dense.txt
    umount_ntfs
}

build_ads() {
    local img=ntfs-ads.img
    echo "[vm] $img"
    rm -f $img
    truncate -s 16M $img
    mkntfs -q -F -f -L "ADSTest" -c 4096 --with-uuid $img
    mount_ntfs $img
    printf 'primary data\n' > $MNT/tagged.txt
    # Named $DATA attributes via the streams_interface=windows path syntax.
    # fs-ntfs should expose these as secondary Data attribute streams.
    printf 'alice author stream\n' > "$MNT/tagged.txt:author"
    printf 'one-line summary\n'    > "$MNT/tagged.txt:summary"
    printf 'no streams here\n' > $MNT/plain.txt
    umount_ntfs
}

build_unicode() {
    local img=ntfs-unicode.img
    echo "[vm] $img"
    rm -f $img
    truncate -s 16M $img
    mkntfs -q -F -f -L "Unicode" -c 4096 --with-uuid $img
    mount_ntfs $img
    # Filenames hit three UTF-16 ranges: BMP Latin (German umlaut), CJK
    # (Japanese), and astral (emoji → UTF-16 surrogate pair).
    printf 'umlaut\n'   > "$MNT/grüße.txt"
    printf 'japanese\n' > "$MNT/日本語.txt"
    printf 'emoji\n'    > "$MNT/hello-🌍.txt"
    mkdir -p "$MNT/папка"
    printf 'cyrillic dir\n' > "$MNT/папка/file.txt"
    umount_ntfs
}

build_deep() {
    local img=ntfs-deep.img
    echo "[vm] $img"
    rm -f $img
    truncate -s 16M $img
    mkntfs -q -F -f -L "Deep" -c 4096 --with-uuid $img
    mount_ntfs $img
    # 20-level nested path — exercises repeated directory-index walks.
    path=$MNT
    i=1
    while [ $i -le 20 ]; do
        path=$path/level$i
        mkdir -p $path
        i=$((i + 1))
    done
    printf 'deep file content\n' > $path/buried.txt
    printf 'surface\n' > $MNT/surface.txt
    umount_ntfs
}

# --- dispatch -------------------------------------------------------------

ALL="basic manyfiles large-file sparse ads unicode deep"
TARGETS="${*:-$ALL}"

for t in $TARGETS; do
    case "$t" in
        basic)      build_basic ;;
        manyfiles)  build_manyfiles ;;
        large-file) build_large_file ;;
        sparse)     build_sparse ;;
        ads)        build_ads ;;
        unicode)    build_unicode ;;
        deep)       build_deep ;;
        *)          echo "[vm] unknown target: $t (have: $ALL)" >&2; exit 1 ;;
    esac
done

echo "[vm] done — syncing."
sync
