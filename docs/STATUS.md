# fs-ntfs — status & upgrade notes

Snapshot of where the crate is today, what it actually exposes, and
what a future agent should tackle first to turn it from a thin
read-only probe into a more capable NTFS driver.

## Architecture

Three layers:

```
   ┌─────────────────────────────────────────────────────────┐
   │ non-Rust callers (Swift/C/Go/…)                         │
   │ — link libfs_ntfs.a, include fs_ntfs.h                  │
   └──────────────────────┬──────────────────────────────────┘
                          │ C ABI (fs_ntfs_*)
   ┌──────────────────────▼──────────────────────────────────┐
   │ src/lib.rs (this crate, ~720 LOC)                       │
   │ — opaque handles, callback block device adapter,        │
   │   thread-local last_error, path-based navigation        │
   └──────────────────────┬──────────────────────────────────┘
                          │ Rust API
   ┌──────────────────────▼──────────────────────────────────┐
   │ ntfs = "0.4"  (ColinFinck/ntfs, crates.io)              │
   │ — MFT parsing, attribute reads, file/dir indexes        │
   └─────────────────────────────────────────────────────────┘
```

The Rust code here is almost entirely glue: marshalling paths, walking
directory indexes, wiring a `Read + Seek` reader (either a `File`
wrapper or a C-callback wrapper) into upstream, and shaping the
results into `#[repr(C)]` structs that FFI callers can consume.

Anything filesystem-level that isn't implemented here is either a
limitation of upstream or a feature that would need new code in this
crate to surface.

## What the C ABI exposes today

All functions and types prefixed `fs_ntfs_` / `FsNtfs…`. Declared in
[`include/fs_ntfs.h`](../include/fs_ntfs.h); implemented in
[`src/lib.rs`](../src/lib.rs).

### Lifecycle

| Function | Role | Notes |
|---|---|---|
| `fs_ntfs_mount(device_path)` | Open + parse an NTFS volume from a path | Uses `std::fs::File` directly; only useful to callers that can open files (not sandboxed FSKit extensions). |
| `fs_ntfs_mount_with_callbacks(cfg)` | Mount through a caller-supplied read callback | Sandbox-safe path. `cfg.read(ctx, buf, offset, length)` returns 0 on success, -errno on failure. |
| `fs_ntfs_umount(fs)` | Free the handle + drop the underlying reader | Must be called exactly once per successful mount. |
| `fs_ntfs_last_error()` | Thread-local `errno`-like reason string | Updated on every failing call; valid until the next FFI call on the same thread. |

### Reads

| Function | Role |
|---|---|
| `fs_ntfs_get_volume_info(fs, out)` | Label, cluster size, total/used clusters, version. |
| `fs_ntfs_stat(fs, path, out)` | Per-file metadata — size, timestamps, file type. |
| `fs_ntfs_dir_open/next/close(fs, path)` | Streaming directory iterator. One entry at a time; `_next` returns NULL when exhausted. |
| `fs_ntfs_read_file(fs, path, offset, length, buf)` | Pread-style data read. Returns bytes read or -1 on error. |
| `fs_ntfs_readlink(fs, path, buf, cap)` | Reparse-point / symlink target. See limitations below. |

### Data types

- `FsNtfsHandle` — opaque handle to a mounted filesystem.
- `FsNtfsAttr` — file metadata (size, timestamps, file_type, etc).
- `FsNtfsDirent` — one entry in a directory listing.
- `FsNtfsVolumeInfo` — volume-scoped info.
- `FsNtfsBlockdevCfg` — read callback + context + size for the callback mount path.
- `FsNtfsDirIter` — opaque directory iterator (heap-allocated, must be closed).

## Known limitations

### Not implemented in this crate (would require code here)

- **Writes of any kind.** All APIs are read-only. Upstream `ntfs` does
  not expose mutating operations at all in 0.4, so this isn't a small
  addition.
- **Alternate Data Streams (ADS).** NTFS files can have named data
  streams (`file.txt:stream_name`). The read-file API only reads the
  default unnamed data attribute. An API extension along the lines of
  `fs_ntfs_read_stream(fs, path, stream_name, …)` would be the
  idiomatic way to surface this.
- **Extended attributes / `$EA`.** Not surfaced.
- **Per-file NTFS permissions / ACLs.** Stat returns a simplified POSIX
  mode; native NTFS ACLs are ignored.
- **Reparse points / symlink target resolution.** `fs_ntfs_readlink`
  exists but only handles trivial symlink reparse-point tags; junctions
  and mount points need specialised handling that isn't implemented.
- **Sparse files.** Data reads will zero-fill holes correctly (upstream
  handles that), but there is no API to enumerate sparse ranges / use
  `SEEK_HOLE`/`SEEK_DATA`.
- **Compressed files.** Transparent decompression works through
  upstream; there is no API to report the compressed vs uncompressed
  size separately.
- **Encrypted files (EFS).** Upstream does not implement EFS decryption;
  reads on EFS-protected files will fail.

### Upstream limitations (would require work in ColinFinck/ntfs first)

- **Write support.** Not on their roadmap.
- **`$LogFile` replay.** Upstream parses cleanly-unmounted volumes
  only; dirty volumes may fail or return stale data. A clean read of an
  actively-mounted volume is out of scope.
- **Very large volumes.** Well-tested for typical partition sizes; the
  edge of the 2^48-cluster limit hasn't been exercised.

### Crate-level items that are sloppy and worth cleaning up

- `best_file_name_str` in `src/lib.rs` is dead code (emits a
  compile warning). The logic picking the preferred filename namespace
  (Win32 > DOS+Win32 > DOS > POSIX) is correct; wire it up or delete.
- No clippy gate in CI yet; there are a few `needless_to_owned` /
  unused-import warnings.
- Integration tests depend on a test image at `/tmp/test_ntfs.img`.
  Ported from the upstream ntfsbridge crate as-is; a cleaner design
  would take a path via an env var or generate a small image inline
  under `OUT_DIR`.
- `fs_ntfs_read_file` copies through an intermediate buffer; direct
  scatter-read into the caller's buffer is doable but wasn't
  implemented.
- Thread-local `last_error` is single-string; some consumers would
  benefit from a structured errno companion (`fs_ntfs_last_errno()`)
  matching the pattern in `fs-ext4`.

## Test coverage

6 integration tests in `tests/integration.rs`:

- `test_volume_info` — volume name + NTFS version.
- `test_root_directory` — root listing includes expected names.
- `test_subdirectory_listing` — nested dir listing.
- `test_stat_file` — `fs_ntfs_stat` returns non-zero size + timestamps.
- `test_read_file_content` — byte-identical read of a known file.
- `test_read_nested_file` — read through a subdirectory.

All 6 pass against `/tmp/test_ntfs.img` (ntfs-basic.img fixture).

No unit tests inside the crate; all coverage is integration-level.

## Suggested upgrade order (for a future agent)

Biggest bang-for-buck first:

1. **Structured errno companion.** Add `fs_ntfs_last_errno()` returning
   POSIX errno (ENOENT, EIO, EINVAL, etc) alongside the string
   `last_error()`. Mirrors `fs_ext4_last_errno()`. FFI callers want both.
2. **ADS read API.** `fs_ntfs_read_stream(fs, path, stream_name, …)` +
   `fs_ntfs_list_streams(…)`. Upstream exposes attribute enumeration so
   this is mostly plumbing.
3. **Reparse-point handling.** Proper `fs_ntfs_readlink` that resolves
   symlinks, junctions, and mount-point reparse tags.
4. **Test fixture hygiene.** Stop depending on `/tmp/test_ntfs.img`;
   either generate an image in `build.rs` / a test-support crate, or
   take the path via `NTFS_TEST_IMAGE` env var.
5. **Clippy gate in CI.** Fix the existing warnings, turn on
   `-D warnings` in `.github/workflows/ci.yml`.
6. **Universal release builds on tag push.** The release workflow is
   cloned from fs-ext4; verify it produces a working
   `libfs_ntfs-v0.1.0-macos-universal.tar.gz` on a tag.
7. **Rust-native facade.** The current Rust API is mostly FFI-shaped
   (raw pointers, `*mut FsNtfsHandle`). Add a thin idiomatic wrapper
   (`Filesystem::mount(path) -> Result<Filesystem>`, `File::read` etc)
   so Rust consumers don't have to go through the C ABI.
8. **Writes.** Only attempt this after upstream `ntfs` supports writes,
   or after forking. Not a near-term goal.

## How this crate is consumed today

- `github.com/christhomas/ext4-fskit` (archived) — `ntfsfskitd`
  extension target. Links `vendor/fs_ntfs/libfs_ntfs.a`, bridging
  header imports `fs_ntfs.h`.
- `github.com/christhomas/diskjockey` — `DiskJockeyNTFS` extension
  target. Same link shape. Uses the callback mount path
  (`fs_ntfs_mount_with_callbacks`) because the extension is sandboxed
  and can't open `/dev/diskN` directly.

If you change the C ABI (add functions, rename symbols, change struct
layouts), both consumers need a follow-up commit. The safest policy is
strictly additive changes until the crate hits 1.0.
