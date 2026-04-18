#!/bin/bash
# Build the NTFS test-disk fixtures inside a qemu-hosted Alpine Linux VM.
#
# Why qemu: mkntfs + ntfs-3g loop-mount are Linux-only. qemu works everywhere
# (macOS, Linux, CI), so one script drives the build on any host. Nothing
# about fs-ntfs itself touches platform specifics; this is just a
# build-time convenience for populating image fixtures.
#
# ntfs-3g is used ONLY during fixture creation (the exact parallel to
# `mount -t ext4 -o loop` in rust-fs-ext4's builder). The test binary
# never touches it — `cargo test` opens the .img files raw through the
# fs-ntfs crate.
#
# First run downloads Alpine's virt ISO + ntfs-3g apks (~50 MB total)
# into .vm-cache/. Subsequent runs reuse the cache.
#
# Usage:
#   bash build-ntfs-feature-images.sh                 # build all images
#   bash build-ntfs-feature-images.sh basic manyfiles # build named ones
#
# Requires: qemu-system-x86_64, curl, tar, bsdtar (libarchive-tools).
# Available on macOS (brew install qemu libarchive), ubuntu-latest
# (apt install qemu-system-x86 libarchive-tools), alpine, fedora.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

CACHE="$SCRIPT_DIR/.vm-cache"
mkdir -p "$CACHE"

# ---------------------------------------------------------------------------
# Step 1 — pin Alpine version + download netboot assets on first run.
# ---------------------------------------------------------------------------
ALPINE_VER=3.21.4
ALPINE_REL="${ALPINE_VER%.*}"
ALPINE_ISO="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_REL}/releases/x86_64/alpine-virt-${ALPINE_VER}-x86_64.iso"
ALPINE_MAIN="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_REL}/main/x86_64"

download_if_missing() {
    local url="$1" out="$2"
    if [ ! -s "$out" ]; then
        echo "[host] downloading $(basename "$out")..."
        curl -fsSL -o "$out" "$url"
    fi
}

# Resolve the newest apk filename matching `<name>-<digit>...` on the
# Alpine CDN autoindex. Avoids pinning versions that rot across Alpine
# point releases.
resolve_apk() {
    local name="$1"
    local listing
    listing="$(curl -fsSL "$ALPINE_MAIN/" || true)"
    printf '%s\n' "$listing" \
        | grep -oE "${name}-[0-9][^\"]*\.apk" \
        | sort -u | sort -V | tail -1
}

download_if_missing "$ALPINE_ISO" "$CACHE/alpine-virt.iso"

# Extract ISO kernel + initramfs. Same rationale as rust-fs-ext4:
# ISO initramfs expects a local apk cache at /media/cdrom, which suits
# our offline-base approach.
if [ ! -s "$CACHE/vmlinuz-virt" ] || [ ! -s "$CACHE/initramfs-virt" ]; then
    echo "[host] extracting kernel + initramfs from alpine-virt ISO..."
    bsdtar -xf "$CACHE/alpine-virt.iso" -C "$CACHE" \
        boot/vmlinuz-virt boot/initramfs-virt
    cp "$CACHE/boot/vmlinuz-virt"   "$CACHE/vmlinuz-virt"
    cp "$CACHE/boot/initramfs-virt" "$CACHE/initramfs-virt"
fi

# ntfs-3g + deps. Not shipped in the alpine-virt ISO; downloaded separately
# and extracted in-place by the local.d wrapper (same trick ext4's builder
# uses for attr/acl). Versions auto-discovered — Alpine point releases do
# not guarantee apk filenames stay identical.
mkdir -p "$CACHE/extra-apks"
resolve_and_download() {
    local name="$1"
    local fname
    fname="$(resolve_apk "$name")"
    if [ -z "$fname" ]; then
        echo "[host] ERROR: could not resolve '$name' on $ALPINE_MAIN" >&2
        exit 1
    fi
    download_if_missing "$ALPINE_MAIN/$fname" "$CACHE/extra-apks/$fname"
}
resolve_and_download "ntfs-3g"
resolve_and_download "ntfs-3g-libs"
resolve_and_download "ntfs-3g-progs"
# Alpine 3.21's ntfs-3g compiles libfuse in statically, so no runtime
# fuse apk is required. Only transitive shared-lib dep beyond musl libc
# is libuuid (ntfs-3g-progs links it for volume UUID generation).
resolve_and_download "libuuid"

# ---------------------------------------------------------------------------
# Step 2 — assemble the apkovl (Alpine overlay) that wires our guest
# builder in as an auto-started local.d service.
# ---------------------------------------------------------------------------
OVL_TMP="$CACHE/ovl"
rm -rf "$OVL_TMP"
mkdir -p \
    "$OVL_TMP/etc/local.d" \
    "$OVL_TMP/etc/runlevels/sysinit" \
    "$OVL_TMP/etc/runlevels/boot" \
    "$OVL_TMP/etc/runlevels/default" \
    "$OVL_TMP/etc/apk"

for svc in devfs dmesg mdev hwdrivers modloop; do
    ln -sf /etc/init.d/"$svc" "$OVL_TMP/etc/runlevels/sysinit/$svc"
done
for svc in bootmisc hostname hwclock modules sysctl syslog urandom; do
    ln -sf /etc/init.d/"$svc" "$OVL_TMP/etc/runlevels/boot/$svc"
done

# Packages diskless-init pre-installs (from the CDROM local repo).
# Keep it minimal — ntfs-3g arrives via the extra-apks tarball extract.
cat > "$OVL_TMP/etc/apk/world" <<'PKGS_EOF'
alpine-base
busybox
PKGS_EOF

cat > "$OVL_TMP/etc/apk/repositories" <<'REPO_EOF'
/media/cdrom/apks
REPO_EOF

# local.d wrapper — mounts /host via 9p, extracts extra-apks in place
# (ntfs-3g + fuse2), then dispatches to the real fixture builder.
cat > "$OVL_TMP/etc/local.d/99-ntfs.start" <<'WRAPPER_EOF'
#!/bin/sh
exec > /dev/console 2>&1

echo "=== [vm] local.d starting ==="

modprobe 9p 9pnet 9pnet_virtio loop fuse 2>/dev/null || true

mkdir -p /host
if ! mount -t 9p -o trans=virtio,version=9p2000.L,msize=131072 host /host; then
    echo "=== [vm] 9p mount failed — aborting ==="
    poweroff -f
fi
echo "=== [vm] /host mounted ==="

for pkg in /host/.vm-cache/extra-apks/*.apk; do
    echo "=== [vm] extracting $(basename "$pkg") ==="
    tar -xzf "$pkg" -C / --exclude=.PKGINFO --exclude=.SIGN.\* \
        --exclude=.pre-install --exclude=.post-install \
        --exclude=.pre-upgrade --exclude=.post-upgrade 2>/dev/null || true
done

# fuse2 ships a setuid helper and /dev/fuse; ensure both exist.
[ -c /dev/fuse ] || mknod -m 0666 /dev/fuse c 10 229

echo "=== [vm] running _vm-builder.sh ==="
if sh /host/_vm-builder.sh $(cat /host/.vm-cache/vm-args 2>/dev/null) \
        > /host/.vm-cache/vm-build.log 2>&1; then
    touch /host/.vm-cache/vm-build.done
    echo "=== [vm] builder succeeded ==="
else
    touch /host/.vm-cache/vm-build.failed
    echo "=== [vm] builder FAILED ==="
    tail -n 30 /host/.vm-cache/vm-build.log
fi

sync
poweroff -f
WRAPPER_EOF
chmod +x "$OVL_TMP/etc/local.d/99-ntfs.start"

ln -sf /etc/init.d/local "$OVL_TMP/etc/runlevels/default/local"

# Apkovl as a 2nd CDROM (Alpine auto-applies localhost.apkovl.tar.gz found
# on any mounted fs at boot).
OVL_STAGE="$CACHE/ovl-iso-stage"
rm -rf "$OVL_STAGE" "$CACHE/ovl.iso" "$CACHE/vm-build.done" "$CACHE/vm-build.failed" "$CACHE/vm-build.log"
mkdir -p "$OVL_STAGE"
(cd "$OVL_TMP" && tar -czf "$OVL_STAGE/localhost.apkovl.tar.gz" etc)
bsdtar -c -f "$CACHE/ovl.iso" --format=iso9660 -C "$OVL_STAGE" .

# ---------------------------------------------------------------------------
# Step 3 — boot Alpine under qemu with a 9p share of this directory.
# ---------------------------------------------------------------------------
echo "[host] booting Alpine under qemu (serial -> stdout)..."

# Pass the requested image-name list through to the guest via the 9p share.
printf '%s\n' "$@" > "$CACHE/vm-args"

qemu-system-x86_64 \
    -kernel "$CACHE/vmlinuz-virt" \
    -initrd "$CACHE/initramfs-virt" \
    -append "console=ttyS0 modules=loop,squashfs,sd-mod,usb-storage,virtio_blk,virtio_net,virtio_pci,9p,9pnet_virtio,fuse" \
    -drive file="$CACHE/alpine-virt.iso",media=cdrom,readonly=on,if=ide,index=0 \
    -drive file="$CACHE/ovl.iso",media=cdrom,readonly=on,if=ide,index=1 \
    -virtfs local,path="$SCRIPT_DIR",mount_tag=host,security_model=mapped-xattr,id=host \
    -m 1024 \
    -smp 2 \
    -nographic \
    -no-reboot

# ---------------------------------------------------------------------------
# Step 4 — inspect the done-marker the guest left behind.
# ---------------------------------------------------------------------------
if [ -f "$CACHE/vm-build.done" ]; then
    echo "[host] guest reported success."
    exit 0
elif [ -f "$CACHE/vm-build.failed" ]; then
    echo "[host] guest reported failure. Last 50 lines of vm-build.log:" >&2
    tail -n 50 "$CACHE/vm-build.log" >&2 || true
    exit 1
else
    echo "[host] guest exited without writing a done marker — something" >&2
    echo "       went wrong during boot. Check earlier serial output." >&2
    exit 1
fi
