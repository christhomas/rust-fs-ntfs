#!/usr/bin/env bash
# Build the NTFS read-path test-disk fixtures (test-disks/ntfs-*.img).
#
# The fixture-driven integration tests (tests/ads.rs, tests/unicode.rs,
# tests/deep.rs, …) open pre-built NTFS images raw through the crate's own
# reader. Those images are intentionally NOT committed (large binaries,
# listed in .gitignore) — they are generated here.
#
# How it works
# ------------
# Each fixture is formatted with `mkntfs` and then populated by mounting it
# with `ntfs-3g` and copying known content in. `ntfs-3g` is used ONLY here,
# at fixture-creation time — the exact parallel to `mount -t ext4 -o loop`
# in rust-fs-ext4's builder. The test binary never touches it: `cargo test`
# opens the .img files raw through the fs-ntfs crate.
#
# Platform
# --------
# `mkntfs` + an `ntfs-3g` loop-mount are Linux-only. On a Linux host (incl.
# ubuntu CI runners) this runs the builders natively — no VM. The tools come
# from the `ntfs-3g` package:
#
#   apt-get install -y ntfs-3g       # Debian / Ubuntu  (mkntfs + ntfs-3g)
#
# Mounting needs root, so the mount/umount steps run under `sudo` when the
# script is not already root.
#
# On macOS there is no native mkntfs/ntfs-3g; build the fixtures on a Linux
# box (or in CI) and copy them over, or run this inside a Linux container.
#
# Usage
#   bash test-disks/build-ntfs-feature-images.sh                 # all images
#   bash test-disks/build-ntfs-feature-images.sh basic ads       # named ones

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

# ---------------------------------------------------------------------------
# Tool discovery. mkntfs + ntfs-3g must be on PATH (they live in /sbin on
# some distros, which isn't always on a non-login PATH).
# ---------------------------------------------------------------------------
export PATH="$PATH:/sbin:/usr/sbin"

if [ "$(uname -s)" != "Linux" ]; then
    echo "ERROR: this builder needs Linux (mkntfs + ntfs-3g loop-mount)." >&2
    echo "       Run it on a Linux host / container / CI runner." >&2
    exit 1
fi

for tool in mkntfs ntfs-3g truncate dd; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "ERROR: '$tool' not found on PATH." >&2
        echo "       Install the fixture toolchain:  apt-get install -y ntfs-3g" >&2
        exit 1
    fi
done

# Root is required to loop-mount. Re-exec individual mount/umount under sudo
# when we are not already uid 0.
SUDO=""
if [ "$(id -u)" -ne 0 ]; then
    if command -v sudo >/dev/null 2>&1; then
        SUDO="sudo"
    else
        echo "ERROR: must run as root (loop-mount) and 'sudo' is unavailable." >&2
        exit 1
    fi
fi

MNT="$(mktemp -d "${TMPDIR:-/tmp}/ntfs-fixture.XXXXXX")"
cleanup() {
    # Best-effort unmount on any exit path so a failed builder doesn't leave
    # a dangling FUSE mount behind.
    if mountpoint -q "$MNT" 2>/dev/null; then
        $SUDO umount "$MNT" 2>/dev/null || true
    fi
    rmdir "$MNT" 2>/dev/null || true
}
trap cleanup EXIT

mount_ntfs() {
    # streams_interface=windows lets us write alternate data streams via
    # path:stream syntax. big_writes improves throughput for large fixtures.
    $SUDO ntfs-3g -o streams_interface=windows,big_writes,default_permissions \
        "$1" "$MNT"
}

umount_ntfs() {
    sync
    $SUDO umount "$MNT"
    # ntfs-3g unmount is async; wait for the FUSE handle to drop before
    # touching the image again.
    while pgrep -x ntfs-3g >/dev/null 2>&1; do
        sleep 0.1
    done
}

# `as_user <file> <cmd...>` — run a populate command that writes through the
# mount. The mount is owned by root (we mounted under sudo), so writes must
# also go through sudo. We funnel everything through `sudo sh -c` so shell
# redirection lands on the root-owned mount.
run_root() {
    $SUDO sh -c "$1"
}

# --- image builders -------------------------------------------------------

build_basic() {
    local img=ntfs-basic.img
    echo "[build] $img"
    rm -f "$img"
    truncate -s 16M "$img"
    mkntfs -q -F -f -L "BasicNTFS" -c 4096 --with-uuid "$img"
    mount_ntfs "$img"
    run_root "printf 'Hello from NTFS!\n' > '$MNT/hello.txt'"
    run_root "mkdir -p '$MNT/Documents'"
    run_root "printf 'Test document content\n' > '$MNT/Documents/readme.txt'"
    run_root "printf 'Some notes here.\n'       > '$MNT/Documents/notes.txt'"
    umount_ntfs
}

build_manyfiles() {
    local img=ntfs-manyfiles.img
    echo "[build] $img"
    rm -f "$img"
    truncate -s 32M "$img"
    mkntfs -q -F -f -L "ManyFiles" -c 4096 --with-uuid "$img"
    mount_ntfs "$img"
    run_root "mkdir -p '$MNT/bigdir'
              i=1
              while [ \$i -le 512 ]; do
                  printf 'content of file %03d\n' \$i > '$MNT'/bigdir/file_\$i.txt
                  i=\$((i + 1))
              done
              printf 'control\n' > '$MNT/small.txt'"
    umount_ntfs
}

build_large_file() {
    local img=ntfs-large-file.img
    echo "[build] $img"
    rm -f "$img"
    truncate -s 64M "$img"
    mkntfs -q -F -f -L "LargeFile" -c 4096 --with-uuid "$img"
    mount_ntfs "$img"
    # 8 MB of zeros, then stamp a distinct ASCII byte ('A'..'H') at the start
    # of each 1 MB block. Tests read those marker bytes to verify offset-
    # correct reads across a non-resident $DATA attribute.
    run_root "dd if=/dev/zero of='$MNT/big.bin' bs=1M count=8 status=none
              i=0
              while [ \$i -lt 8 ]; do
                  off=\$((i * 1048576))
                  byte=\$(printf '\\\\%03o' \$((0101 + i)))
                  printf \"\$byte\" | dd of='$MNT/big.bin' bs=1 count=1 seek=\$off conv=notrunc status=none
                  i=\$((i + 1))
              done
              printf 'small control file\n' > '$MNT/small.txt'"
    umount_ntfs
}

build_sparse() {
    local img=ntfs-sparse.img
    echo "[build] $img"
    rm -f "$img"
    truncate -s 32M "$img"
    mkntfs -q -F -f -L "Sparse" -c 4096 --with-uuid "$img"
    mount_ntfs "$img"
    # 4 MB file with holes: 1 byte every 1 MB, truncated to 4 MB.
    run_root "dd if=/dev/zero of='$MNT/sparse.bin' bs=1 count=0 seek=4M status=none
              off=0
              while [ \$off -lt 4000000 ]; do
                  printf 'X' | dd of='$MNT/sparse.bin' bs=1 count=1 seek=\$off conv=notrunc status=none
                  off=\$((off + 1048576))
              done
              printf 'dense\n' > '$MNT/dense.txt'"
    umount_ntfs
}

build_ads() {
    local img=ntfs-ads.img
    echo "[build] $img"
    rm -f "$img"
    truncate -s 16M "$img"
    mkntfs -q -F -f -L "ADSTest" -c 4096 --with-uuid "$img"
    mount_ntfs "$img"
    run_root "printf 'primary data\n' > '$MNT/tagged.txt'"
    # Named $DATA attributes via the streams_interface=windows path syntax.
    run_root "printf 'alice author stream\n' > '$MNT/tagged.txt:author'"
    run_root "printf 'one-line summary\n'    > '$MNT/tagged.txt:summary'"
    run_root "printf 'no streams here\n' > '$MNT/plain.txt'"
    umount_ntfs
}

build_unicode() {
    local img=ntfs-unicode.img
    echo "[build] $img"
    rm -f "$img"
    truncate -s 16M "$img"
    mkntfs -q -F -f -L "Unicode" -c 4096 --with-uuid "$img"
    mount_ntfs "$img"
    # Filenames hit three UTF-16 ranges: BMP Latin (umlaut), CJK (Japanese),
    # and astral (emoji -> UTF-16 surrogate pair), plus a Cyrillic dir.
    run_root "printf 'umlaut\n'   > '$MNT/grüße.txt'
              printf 'japanese\n' > '$MNT/日本語.txt'
              printf 'emoji\n'    > '$MNT/hello-🌍.txt'
              mkdir -p '$MNT/папка'
              printf 'cyrillic dir\n' > '$MNT/папка/file.txt'"
    umount_ntfs
}

build_deep() {
    local img=ntfs-deep.img
    echo "[build] $img"
    rm -f "$img"
    truncate -s 16M "$img"
    mkntfs -q -F -f -L "Deep" -c 4096 --with-uuid "$img"
    mount_ntfs "$img"
    # 20-level nested path — exercises repeated directory-index walks.
    run_root "path='$MNT'
              i=1
              while [ \$i -le 20 ]; do
                  path=\$path/level\$i
                  mkdir -p \"\$path\"
                  i=\$((i + 1))
              done
              printf 'deep file content\n' > \"\$path/buried.txt\"
              printf 'surface\n' > '$MNT/surface.txt'"
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
        *)          echo "unknown target: $t (have: $ALL)" >&2; exit 1 ;;
    esac
done

echo "[build] done — fixtures in $SCRIPT_DIR"
ls -lh "$SCRIPT_DIR"/ntfs-*.img 2>/dev/null || true
