# fs-ntfs

Pure-Rust NTFS driver with a stable C ABI (`fs_ntfs_*`) designed to
be linked from C, C++, Go (via CGo), or any other language with FFI.
Portable cargo crate — no platform-specific dependencies.

Matches the layout of its sibling crate
[rust-fs-ext4](https://github.com/christhomas/rust-fs-ext4) — same
shape, different filesystem.

## Origins

This crate began as a *very thin* C-ABI wrapper around
[ColinFinck/ntfs](https://github.com/ColinFinck/ntfs). Symbols are
named `fs_ntfs_*`, the cargo dependency points at the crates.io
release of `ntfs = "0.4"`, and the header lives at `fs_ntfs.h`. It
is used in production by
[DiskJockey](https://github.com/christhomas/diskjockey), but carries
no coupling back to that project — any FFI host can consume it.

## What this adds over upstream `ntfs`

Upstream `ntfs` is a well-engineered, **read-only** pure-Rust NTFS
parser. fs-ntfs depends on it (unchanged) for all read paths and
then layers a large amount of original work on top. The net effect
is that fs-ntfs is a read/write NTFS driver with recovery, rather
than a read-only parser.

What's here that upstream does not provide:

| Area | fs-ntfs | upstream `ntfs = "0.4"` |
|---|---|---|
| Mount + navigate (stat / readdir / read) | ✓ (via upstream) | ✓ |
| Resident-attribute reads | ✓ (via upstream) | ✓ |
| Non-resident / extent-mapped reads | ✓ (via upstream) | ✓ |
| `$INDEX_ROOT` + `$INDEX_ALLOCATION` iteration | ✓ (via upstream) | ✓ |
| Symlinks, reparse points, ADS reads | ✓ (via upstream) | ✓ |
| **File content writes** (resident, non-resident, offset-patches) | ✓ | ✗ |
| **Resident → non-resident promotion** | ✓ | ✗ |
| **Non-resident `$DATA` grow / truncate** | ✓ | ✗ |
| **Cluster allocation** against `$Bitmap` | ✓ | ✗ |
| **MFT-record allocation** against `$MFT:$Bitmap` | ✓ | ✗ |
| **FILE-record assembly + fixup array** | ✓ | ✗ |
| **Create / unlink / mkdir / rmdir** | ✓ | ✗ |
| **Hard links (`$FILE_NAME` fan-out + link-count bookkeeping)** | ✓ | ✗ |
| **Rename (same-length + variable-length, index rewrite)** | ✓ | ✗ |
| **`$INDEX_ROOT` insert / remove** | ✓ | ✗ |
| **`chattr` (`FILE_ATTRIBUTE_*` flag toggling)** | ✓ | ✗ |
| **Timestamp writes (atime / mtime / ctime / crtime)** | ✓ | ✗ |
| **Reparse-point write / remove + symlink create** | ✓ | ✗ |
| **Extended-attribute write / remove** | ✓ | ✗ |
| **Alternate data stream write / delete (resident)** | ✓ | ✗ |
| **`$OBJECT_ID` read** | ✓ | ✗ |
| **Dirty-flag detection + clear** | ✓ | ✗ |
| **`$LogFile` reset** | ✓ | ✗ |
| **End-to-end `fsck`** | ✓ | ✗ |
| **Free-cluster + free-MFT-record statistics** | ✓ | ✗ |
| **Block-device callback transport** (for hosts with no file access) | ✓ | ✗ |
| **Stable C ABI** (`fs_ntfs_*`, `fs_ntfs.h`) | ✓ | ✗ |
| **Thread-local error state with inferred errno** | ✓ | ✗ |
| **Integration test suite for every write path, incl. corruption fuzz** | ✓ | partial |

By line count the write + recovery + FFI code (`write.rs`,
`record_build.rs`, `attr_resize.rs`, `index_io.rs`, `bitmap.rs`,
`mft_bitmap.rs`, `fsck.rs`, `ea_io.rs`, plus the FFI surface in
`lib.rs`) is larger than the read/glue layer. The **read** paths
still go through ColinFinck's crate unchanged — that remains
excellent and is used verbatim for MFT parsing, attribute decoding,
and directory-index traversal.

The short version: if you need read-only NTFS in pure Rust, use
upstream directly. If you need a read/write driver with a stable C
ABI, `fsck`, and a block-device callback transport for hosts that
can't open a file directly, that's what fs-ntfs adds.

## Architecture

```
   ┌──────────────────────────────────────────────────────┐
   │ Non-Rust callers (C / C++ / Go / …)                  │
   │  — link libfs_ntfs.a, include fs_ntfs.h              │
   └────────────────────────┬─────────────────────────────┘
                            │ C ABI (fs_ntfs_*)
   ┌────────────────────────▼─────────────────────────────┐
   │ fs-ntfs  (this crate)                                │
   │ ─────────────────────────────────────────────────────│
   │  lib.rs           FFI surface, opaque handles,       │
   │                   thread-local last_error,           │
   │                   path- and callback-based I/O       │
   │                                                      │
   │  write.rs         data writes + file content I/O     │
   │  record_build.rs  FILE-record assembly + fixups      │
   │  attr_io.rs       resident / non-resident attrs      │
   │  attr_resize.rs   resident ↔ non-resident promotion  │
   │  index_io.rs      $INDEX_ROOT / $INDEX_ALLOCATION    │
   │  data_runs.rs     mapping-pairs encode / decode      │
   │  bitmap.rs        $Bitmap cluster allocator          │
   │  mft_bitmap.rs    $MFT:$Bitmap MFT-record allocator  │
   │  ea_io.rs         extended attributes                │
   │  fsck.rs          dirty-flag + $LogFile recovery     │
   │  idx_block.rs     INDX block fixup + traversal       │
   │  upcase.rs        $UpCase collation table            │
   │  facade.rs        ergonomic Rust wrappers            │
   └────────────────────────┬─────────────────────────────┘
                            │ Rust API
   ┌────────────────────────▼─────────────────────────────┐
   │ ntfs = "0.4"  (ColinFinck/ntfs, crates.io)           │
   │  — READ-only MFT / attribute parser                  │
   └──────────────────────────────────────────────────────┘
```

Two transport paths for all I/O:

- **Path-based**: pass a device/image path (`/dev/diskN`, `foo.img`).
  Opens the file with `std::fs::File` directly. Useful for CLI tools,
  tests, and any host that can open a file descriptor.
- **Callback-based**: pass a `fs_ntfs_blockdev_cfg_t` whose `read` /
  `write` function pointers call back into the host to move bytes.
  Required where the host can't open a file directly — sandboxed or
  out-of-process contexts where I/O is mediated by the surrounding
  runtime rather than raw file descriptors.

Every lifecycle + fsck entry point has both variants. Where a
path-only variant still exists (some in-place writers), it's marked
below.

## What the C ABI exposes

Declared in [`include/fs_ntfs.h`](include/fs_ntfs.h); implemented in
[`src/lib.rs`](src/lib.rs). **43 entry points** at the time of writing.

### Lifecycle + error reporting

- `fs_ntfs_mount(path)` / `fs_ntfs_mount_with_callbacks(cfg)`
- `fs_ntfs_umount(fs)`
- `fs_ntfs_get_volume_info(fs, *out)` — label, cluster size, total
  clusters, version, serial, total byte size.
- `fs_ntfs_last_error()`, `fs_ntfs_last_errno()`,
  `fs_ntfs_clear_last_error()` — thread-local error state, with an
  `errno`-style companion heuristically inferred from the message.

### Reads

- `fs_ntfs_stat(fs, path, *out)`
- `fs_ntfs_dir_open` / `fs_ntfs_dir_next` / `fs_ntfs_dir_close`
  (streaming iterator; one entry per `_next`, NULL on end).
- `fs_ntfs_read_file(fs, path, offset, length, buf)` — pread-style.
- `fs_ntfs_readlink(fs, path, buf, bufsize)` — resolves symlinks and
  reparse-point junctions to a target string.

### Recovery / fsck

- `fs_ntfs_is_dirty(path)` / `fs_ntfs_is_dirty_with_callbacks(cfg)`
- `fs_ntfs_clear_dirty(path)` — path-only helper
- `fs_ntfs_reset_logfile(path)` — path-only helper
- `fs_ntfs_fsck(path, *out_logfile_bytes, *out_dirty_cleared)`
- `fs_ntfs_fsck_with_callbacks(cfg, progress_cb, progress_ctx, …)`
  — emits `"reset_logfile"` progress per 64 KiB chunk and a
  start/end `"clear_dirty"` event so long-running repairs can drive
  a host UI.

### Write — metadata

- `fs_ntfs_set_times` — atime / mtime / ctime / crtime (path-based,
  in-place).
- `fs_ntfs_chattr` — add / remove NTFS `FILE_ATTRIBUTE_*` flags
  (READONLY, HIDDEN, SYSTEM, ARCHIVE, …).
- `fs_ntfs_write_reparse_point` / `fs_ntfs_remove_reparse_point`
- `fs_ntfs_create_symlink`
- `fs_ntfs_write_ea` / `fs_ntfs_remove_ea` — extended attributes.
- `fs_ntfs_write_named_stream` / `fs_ntfs_delete_named_stream` —
  Alternate Data Streams (resident).
- `fs_ntfs_read_object_id` — `$OBJECT_ID` reader (useful for stable
  per-file identifiers).

### Write — namespace

- `fs_ntfs_create_file` / `fs_ntfs_unlink`
- `fs_ntfs_mkdir` / `fs_ntfs_rmdir`
- `fs_ntfs_rename` / `fs_ntfs_rename_same_length`
- `fs_ntfs_link` — add a hard link to an existing regular file.

### Write — data

- `fs_ntfs_write_file_contents(path, buf, len)` — replace contents,
  transparently staying resident or promoting to non-resident.
- `fs_ntfs_write_resident_contents(path, buf, len)` — resident-only
  variant for cases where the caller knows the file stays small.
- `fs_ntfs_write_file(path, offset, buf, len)` — size-preserving
  in-place data write into an existing non-resident attribute.
- `fs_ntfs_grow(path, new_size)` / `fs_ntfs_truncate(path, new_size)`
  — extend or shrink non-resident `$DATA`.

### Volume statistics

- `fs_ntfs_free_clusters(path)` — scan `$Bitmap`.
- `fs_ntfs_mft_free_records(path)` — scan `$MFT:$Bitmap`.

## Scope — supported vs. unsupported

**Supported:**

- Read anything upstream `ntfs = "0.4"` can read — including resident
  and non-resident attributes, `$INDEX_ROOT` directories, alternate
  data streams, reparse points, symlinks, junctions.
- All writes listed above against live NTFS images, including
  log-aware recovery before any write is issued.
- Both path- and callback-based I/O across lifecycle + recovery.
- Unicode file names up to the NTFS 255-UTF-16-character limit.

**Intentionally not implemented** (if/when, filed as future work):

- Attribute lists (`$ATTRIBUTE_LIST`) — we reject MFT records that
  overflow into one rather than stitch across records. Affects very
  fragmented files on old, heavily churned volumes.
- `$INDEX_ALLOCATION` overflow in *write* paths. Directory index
  reads across overflow work; inserting / removing entries once a
  directory has overflowed out of `$INDEX_ROOT` is MVP-limited and
  documented per entry point in `fs_ntfs.h`.
- Compressed, encrypted (EFS), and sparse data writes. Reads of
  uncompressed / unencrypted / non-sparse ranges work; writes assume
  plain ranges.
- USN journal updates. Changes are not reflected in `$UsnJrnl`.
- Transactional NTFS (TxF). Deprecated by Microsoft; not a goal.
- Resizing the volume itself (`$Bitmap` grow/shrink).
- Disk-level operations — partitioning, formatting. This crate
  expects a pre-formatted NTFS image; create one with `mkntfs` or
  similar before use.

**Read-only guardrails:**

- `fs_ntfs_mount_with_callbacks` ignores `cfg.write`; the mount
  handle itself is read-only. Mutations go through the separate
  path-based write API against a device/image path — this keeps
  read and write transports orthogonal and makes it easy for a host
  to drive reads via a shared callback while opening the underlying
  device by path for writes.
- Failing writes never leave an image in a state that upstream can't
  re-read. Either the write completed end-to-end or upstream parses
  the image back to its pre-write state. That's a hard invariant,
  covered by the corruption-fuzz + round-trip tests in `tests/`.

## Building

```sh
cargo build --release
# → target/release/libfs_ntfs.a + rlib
```

Universal macOS static lib (aarch64 + x86_64 lipo'd):

```sh
./build.sh           # → dist/libfs_ntfs.a
```

The Rust toolchain is pinned in `rust-toolchain.toml`; `cargo` picks
it up automatically.

## Using from C

Link `libfs_ntfs.a` and include `fs_ntfs.h`. Read example:

```c
#include "fs_ntfs.h"

fs_ntfs_fs_t *fs = fs_ntfs_mount("/path/to/ntfs.img");
if (!fs) {
    fprintf(stderr, "mount: %s\n", fs_ntfs_last_error());
    return 1;
}

fs_ntfs_attr_t attr;
if (fs_ntfs_stat(fs, "/readme.txt", &attr) == 0) {
    printf("size=%llu\n", (unsigned long long)attr.size);
}

fs_ntfs_umount(fs);
```

Callback-based mount for hosts that don't open the device directly
(pseudocode):

```c
static int my_read(void *ctx, void *buf, uint64_t off, uint64_t len) { /* … */ }

fs_ntfs_blockdev_cfg_t cfg = {
    .read       = my_read,
    .context    = my_context,
    .size_bytes = total_bytes,
    /* .write left NULL — read-only mount */
};

fs_ntfs_fs_t *fs = fs_ntfs_mount_with_callbacks(&cfg);
```

End-to-end fsck against a callback-held block device, with progress:

```c
static int on_progress(void *ctx, const char *phase,
                       uint64_t done, uint64_t total) {
    fprintf(stderr, "[fsck] %s %llu/%llu\n", phase,
            (unsigned long long)done, (unsigned long long)total);
    return 0;
}

uint64_t bytes = 0;
uint8_t  cleared = 0;
int rc = fs_ntfs_fsck_with_callbacks(&cfg, on_progress, NULL,
                                     &bytes, &cleared);
```

## Using from Rust

The FFI layer is the primary surface. If you're writing pure Rust
and don't need write support, depend on upstream directly:

```toml
[dependencies]
ntfs = "0.4"
```

If you *do* want fs-ntfs's write API from Rust, pull this crate
in as a path or git dependency and use the modules under `facade::`
and `write::`.

## Testing

```sh
cargo test                       # unit + integration
cargo test --test capi_fsck_callbacks
```

The `tests/` tree carries ~50 integration tests covering reads,
every C-ABI entry point, round-trip writes (including a corruption
fuzz that stress-tests the write paths under concurrent read-back),
Unicode names, large files, sparse reads, deep directories,
rename/resize variants, and fsck on both path and callback
transports. Tests assemble their own NTFS images at runtime where
possible; a handful of static images live in `test-disks/` for
fixtures that are awkward to build programmatically.

## Git hooks

One-time setup per clone, so every commit runs the same `cargo fmt
--check` + `cargo clippy` checks CI does and CI doesn't have to catch
what your machine could have:

```sh
./scripts/install-hooks.sh
```

Bypass a single commit with `git commit --no-verify`.

## Versioning

Semver, tracked in [`CHANGELOG.md`](CHANGELOG.md). ABI-visible
struct fields are appended to preserve binary layout across patch
releases (as happened for `fs_ntfs_blockdev_cfg_t` gaining `write`
in 0.1.1). Breaking ABI changes get a minor bump.

## Credits

Read parsing is the work of [Colin Finck](https://github.com/ColinFinck)
and his [`ntfs`](https://github.com/ColinFinck/ntfs) crate — this
crate depends on it and calls directly into it for every
MFT/attribute/index read. The write, recovery, and FFI code in this
crate is original work, layered on top of those reads.

## License

Dual-licensed under MIT ([LICENSE-MIT](LICENSE-MIT)) or Apache-2.0
([LICENSE-APACHE](LICENSE-APACHE)), matching the upstream `ntfs`
crate.

## Disclaimer — use at your own risk

**Read this before pointing the crate at anything you care about.**

This is experimental filesystem code that reads *and writes* the
on-disk structures of live filesystems. Bugs in this class of code
can — and sooner or later will — corrupt or destroy data. The MIT /
Apache-2.0 license above already contains the standard no-warranty
and limitation-of-liability clauses; this section restates them in
plain English so there is no ambiguity about what you are agreeing
to when you use the software.

**By using this software you accept that:**

- The author(s) and contributors provide this crate **as is**, with
  **no warranty of any kind**, express or implied — including but
  not limited to warranties of merchantability, fitness for a
  particular purpose, correctness, data integrity, durability,
  security, or non-infringement.
- The author(s) and contributors are **not liable** for any loss,
  damage, or expense of any kind arising out of or related to your
  use of the software. This explicitly includes (non-exhaustively)
  lost or corrupted data, corrupted filesystems, volumes that will
  no longer mount, hardware damage, downtime, lost revenue, missed
  deadlines, support costs, or any direct, indirect, incidental,
  special, consequential, or punitive damages — regardless of the
  legal theory under which such damages might be sought.
- You are **solely responsible** for backing up any data that could
  be touched by this software *before* running it. The only safe
  workflow when experimenting with an unofficial filesystem driver
  is: work on disk *images* or on *copies*, never on your only
  copy of anything irreplaceable.
- If that is not acceptable to you, **do not use this software**.

This disclaimer is a plain-English restatement of the license terms
above, not a separate license. The license terms apply in full.
