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

## Test infrastructure

Fixture-driven integration tests. Real NTFS images are generated inside
a qemu-hosted Alpine Linux VM (the only portable way to run
`mkntfs` / loop-mount NTFS on macOS or CI). `ntfs-3g` is used **only
during fixture creation** — the cargo test binary opens the `.img`
files raw via the `ntfs` crate, no FUSE, no kernel driver.

Layout:

```
test-disks/
  build-ntfs-feature-images.sh   # host-side qemu orchestrator
  _vm-builder.sh                 # guest-side mkntfs + mount + populate
  .vm-cache/                     # Alpine ISO + kernel + apks (gitignored)
  ntfs-*.img                     # generated, gitignored
tests/
  common/mod.rs                  # shared open/navigate/read helpers
  integration.rs                 # basic fixture
  manyfiles.rs large_file.rs sparse.rs ads.rs unicode.rs deep.rs
.github/workflows/ci.yml         # installs qemu, runs the script, cargo test
```

Fixtures produced (all via `mkntfs` + `ntfs-3g` mount, all MFT-backed
NTFS 3.1):

| Image | Size | What it exercises |
|---|---|---|
| `ntfs-basic.img` | 16M | Small file, subdir, volume label. Baseline. |
| `ntfs-manyfiles.img` | 32M | 512-entry directory — forces `$INDEX_ALLOCATION` B+ tree (past the resident `$INDEX_ROOT` limit). |
| `ntfs-large-file.img` | 64M | 8 MiB file with marker bytes at each 1 MiB boundary — non-resident `$DATA` with multiple data runs; seek/read across runs. |
| `ntfs-sparse.img` | 32M | 4 MiB file with 1-byte stamps at MiB intervals — holes must zero-fill. |
| `ntfs-ads.img` | 16M | Named `$DATA` streams (`file:author`, `file:summary`). |
| `ntfs-unicode.img` | 16M | BMP Latin, CJK, astral-plane emoji (UTF-16 surrogate pair), Cyrillic dir. |
| `ntfs-deep.img` | 16M | 20-level nested path — repeated directory-index walks from root. |

CI caches `test-disks/.vm-cache/` keyed on the Alpine version so
subsequent runs skip the ~50 MB download. First run is ~60s; cached
runs are ~30s.

**Coverage gap worth flagging.** The tests drive the upstream `ntfs`
crate directly (see `tests/common/mod.rs`), not the `fs_ntfs_*` C ABI
that ships. In practice we're validating the fixtures + upstream, not
this crate's wrapper. A parallel set of `capi_*.rs` tests that exercise
`fs_ntfs_mount` / `fs_ntfs_stat` / `fs_ntfs_dir_open` /
`fs_ntfs_read_file` / `fs_ntfs_mount_with_callbacks` is the obvious
next step — `crate-type = ["staticlib", "rlib"]` permits it.

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

- `best_file_name_str` in `src/lib.rs` carries a drive-by
  `#[allow(dead_code)]` (added so CI's `-D warnings` would pass).
  Logic for picking the preferred filename namespace
  (Win32 > DOS+Win32 > DOS > POSIX) is correct; wire it into
  `fill_attr` / the dir iterator, or delete.
- **Clippy gate disabled in CI.** [`.github/workflows/ci.yml`] currently
  runs only `cargo fmt --check` + `cargo test --release`. Several FFI
  entry points trigger `clippy::not_unsafe_ptr_arg_deref` because
  `pub extern "C" fn` signatures take `*mut` / `*const` and deref them
  without being marked `unsafe`. Fix is mechanical but touches the ABI:
  either mark the functions `unsafe extern "C"` (consumers have to
  follow suit) or add `#[allow(clippy::not_unsafe_ptr_arg_deref)]` at
  the boundary. Either way, resolve before re-enabling
  `cargo clippy --all-targets -- -D warnings`.
- `fs_ntfs_read_file` copies through an intermediate buffer; direct
  scatter-read into the caller's buffer is doable but wasn't
  implemented.
- Thread-local `last_error` is single-string; some consumers would
  benefit from a structured errno companion (`fs_ntfs_last_errno()`)
  matching the pattern in `fs-ext4`.

## Test coverage

39 integration tests across 9 test files. See
[Test infrastructure](#test-infrastructure) above for the fixture
layout; all 39 pass locally and in CI.

| File | Fixture | Tests | What's covered |
|---|---|---|---|
| `integration.rs` | ntfs-basic | 6 | Volume info, root listing, stat file + dir, read root file, read nested file, subdir listing. |
| `manyfiles.rs` | ntfs-manyfiles | 3 | 512-entry listing, read mid-range file, root control file intact. |
| `large_file.rs` | ntfs-large-file | 3 | Non-resident `$DATA` assertion, marker bytes at 1 MiB boundaries (seek+read across data runs), control file. |
| `sparse.rs` | ntfs-sparse | 2 | Logical size, marker + hole-zero-fill reads. |
| `ads.rs` | ntfs-ads | 4 | Primary data unchanged, named-stream enumeration, read named-stream content, file-without-streams has none. |
| `unicode.rs` | ntfs-unicode | 4 | Listing with mixed UTF-16 planes, navigate by unicode name, unicode directory, emoji surrogate-pair roundtrip. |
| `deep.rs` | ntfs-deep | 2 | Read file 20 levels deep, surface file still reads. |
| `fsck.rs` | synth from ntfs-basic | 7 | `fsck::clear_dirty` / `reset_logfile` / `fsck` — dirty state patched by test helpers using upstream-only APIs. |
| `capi_fsck.rs` | synth from ntfs-basic | 8 | Same operations via the `fs_ntfs_*` C ABI; covers null-path, bad-path, return codes, out-params. |

No unit tests inside the crate; all coverage is integration-level.
Coverage gap: see the last paragraph in
[Test infrastructure](#test-infrastructure) — tests drive the upstream
`ntfs` crate, not the `fs_ntfs_*` C ABI.

## Suggested upgrade order (for a future agent)

Revised after the test-infrastructure work and
[Documentation cross-check](#documentation-cross-check) landed. The
cross-check surfaced correctness bugs that outrank any new-feature
work — fix those first.

### Phase W0 — volume recovery (✅ shipped)

First step outside the read-only envelope. Narrow raw-byte writes for
volumes that lost their mount handle without a clean unmount.

- `fs_ntfs_clear_dirty(path)` — clear `VOLUME_IS_DIRTY` (0x0001) in
  `$Volume/$VOLUME_INFORMATION`. Returns `1` (cleared), `0` (already
  clean), `-1` (error).
- `fs_ntfs_reset_logfile(path)` — overwrite `$LogFile` with `0xFF`, the
  format-level "no pending transactions" pattern documented at Flatcap.
  Returns bytes written or `-1`.
- `fs_ntfs_fsck(path, *logfile_bytes, *dirty_cleared)` — both above,
  with optional out-params. `NULL` out-params accepted.

Scope is the weakest possible recovery: no `$LogFile` replay, no
MFT/MFTMirror reconciliation. Uncommitted metadata changes are
discarded — the volume becomes mountable again, but some state may
be lost. This matches the recovery posture of `ntfsfix(8)` (and is
what Windows/ntfs-3g are designed to accept).

**Tests:** `tests/fsck.rs` (7 Rust-layer) + `tests/capi_fsck.rs` (8
C-ABI) — each exercises a different failure mode (dirty flag set,
corrupted log-first-page, combined). Dirty state is synthesized in
the test via upstream-only APIs, independent of the code under test.

### Phase 1 — correctness fixes (block new features until done)

Each item maps to a finding above; the fixture to regression-test
against is named in brackets.

1. **Widen timestamps to 64-bit seconds + nanoseconds.** Covers the
   Y2038 / wraparound / 100 ns-precision loss bugs in one ABI break.
   `[basic]` + a new fixture with a pre-1970 `touch -d` file.
2. **Fix junction / WOF / reparse-point type dispatch.** Read
   `$REPARSE_POINT.tag`; only `IO_REPARSE_TAG_SYMLINK` maps to symlink.
   Ship alongside the `fs_ntfs_readlink` implementation so both land
   together. Needs a new fixture (junction via `ln -T` on an
   ntfs-3g-mounted volume).
3. **Replace seek-by-reading with `NtfsAttributeValue::seek`** in
   `fs_ntfs_read_file`. `[large-file]` — add a test that reads at a
   multi-MiB offset and times out on quadratic behavior.
4. **Normalize path-traversal handling.** Skip `.`; resolve `..` via
   `parent_directory_reference`. Add a capi test that `/a/./b` and
   `/a/../b` resolve correctly.
5. **Handle ADS-only files and widen the dirent name buffer** —
   conceptually one PR: widen `fs_ntfs_dirent_t::name` to 1024 bytes,
   and make `fs_ntfs_read_file` fall back to WOF's `WofCompressedData`
   (or at minimum not error with a zero-length result). `[ads]` +
   `[unicode]` regressions, plus a new 90-character CJK fixture.

### Phase 2 — coverage + reliability scaffolding

6. **`capi_*` tests.** Parallel suite that drives the shipped
   `fs_ntfs_*` C ABI (not upstream `ntfs` directly). Closes the
   reliability gap flagged under [Test infrastructure](#test-infrastructure).
   Same fixtures, tests link the `rlib` half of the crate and call the
   `extern "C" fn`s with `CString` paths / raw-pointer out-params.
7. **Re-enable `clippy -D warnings` in CI.** Resolve the
   `not_unsafe_ptr_arg_deref` lints (mark FFI entries `unsafe extern "C"`
   or allow the lint at the boundary). Without this gate, regressions
   go uncaught.
8. **Dirty-volume state surface.** Add `fs_ntfs_volume_info_t::is_dirty`
   + optional `metadata_consistent` from an MFT-mirror cross-check.
   Requires a fresh fixture with `$Volume` dirty bit set (either force
   via mkntfs options, or a `chattr`-style flag via ntfs-3g).

### Phase 3 — new features

9. **Lazy directory iteration.** Stop materializing entire indexes in
   `dir_open`; retain the iterator. `[manyfiles]` regression + a new
   ≥100k-entry fixture (32 MiB image easily holds it).
10. **ADS read API.** `fs_ntfs_read_stream(fs, path, stream_name, …)` +
    `fs_ntfs_list_streams(…)`. The `ntfs-ads.img` fixture + Rust-layer
    tests already exist; add the C surface + a `capi_ads.rs`.
11. **Structured errno companion.** `fs_ntfs_last_errno()` returning
    POSIX errno. Mirrors `fs_ext4_last_errno()`.
12. **POSIX mode from `FILE_ATTRIBUTE_READONLY` + extensions.** Minimal
    derivation first (drop write bits, set exec for `.exe`/`.bat`/etc);
    full `$SECURITY_DESCRIPTOR` parsing later.
13. **Synthesize `.` / `..` in `dir_open`.** One-liner once parent-ref
    lookup exists.
14. **WOF decompression.** Parse `IO_REPARSE_TAG_WOF` data; decompress
    `WofCompressedData` via `ms-compress` or similar. Needed for
    correctness on modern Win10/11 volumes. Probably biggest single
    engineering task in the list.

### Phase 4 — polish

15. **Wire up or delete `best_file_name_str`.** Strip the
    `#[allow(dead_code)]` drive-by.
16. **Universal release builds on tag push.** Release workflow was
    cloned from fs-ext4; verify it produces
    `libfs_ntfs-v0.1.0-macos-universal.tar.gz` on a tag.
17. **Rust-native facade.** Thin idiomatic wrapper around the C ABI
    (`Filesystem::mount(path) -> Result<Filesystem>`, `File::read`).
18. **Writes.** Only attempt after upstream `ntfs` supports writes,
    or after forking. Not a near-term goal.

## Documentation cross-check

Cross-check against
[MS-FSCC](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-fscc/),
[Flatcap Linux-NTFS project docs](https://flatcap.github.io/linux-ntfs/ntfs/),
and the [upstream `ntfs` 0.4 API](https://docs.rs/ntfs/0.4/ntfs/).
Findings below are in addition to the items already listed under
[Known limitations](#known-limitations).

### Correctness bugs

#### Timestamps truncated to 32-bit UNIX epoch
**Location:** `src/lib.rs:137-141`, `src/lib.rs:187-193`; `include/fs_ntfs.h:37-40`
**Spec:** NTFS timestamps are 64-bit, 100 ns intervals since 1601-01-01 UTC
([MS-DTYP FILETIME](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-dtyp/2c57429b-fdd4-488f-b5fc-9e4cf020fcdf)).
Representable to year 30828 at 100 ns resolution.
**Code:** `secs.saturating_sub(EPOCH_DIFF) as u32`. Sub-second precision is
discarded unconditionally; pre-1970 clamps to 0; values ≥ `u32::MAX`
seconds don't clamp but **truncate via the `as u32` cast** —
2106-02-08 wraps to a small value rather than saturating. Y2038 hits
any signed-int consumer well before that.
**Why it matters:** Silent mtime corruption on archive volumes, backup-tool
sentinel dates (e.g. 2099), and mismatch against SMB/Win32 peers that
compare at 100 ns resolution. Swift/FSKit's `timespec` is 64-bit
`time_t`; we've already narrowed by the time values reach the caller.
**Fix:** Widen the four fields to `int64_t seconds` + `uint32_t nsec`;
convert as `(ts / 10_000_000) as i64 - EPOCH_DIFF` + `((ts % 10_000_000) * 100) as u32`.

#### `fs_ntfs_dirent_t::name[256]` silently truncates legal NTFS names
**Location:** `include/fs_ntfs.h:52`, `src/lib.rs:148-153`, `src/lib.rs:566-576`
**Spec:** `$FILE_NAME` stores up to 255 UTF-16 code units
([Flatcap $FILE_NAME](https://flatcap.github.io/linux-ntfs/ntfs/attributes/file_name.html)).
Worst-case UTF-8 encoding is 255 × 4 = **1020 bytes** plus NUL.
**Code:** `name[256]`; copy capped at 255 bytes with no error signal.
**Why it matters:** A file with non-BMP names (emoji, rare CJK) or long
CJK names (as few as 86 characters exceeds 255 UTF-8 bytes) comes back
with a corrupted, non-roundtrippable name. A subsequent `fs_ntfs_stat`
on that truncated name fails with ENOENT. This is a **silent data-loss
path** hit by any FSKit enumeration of user profile / OneDrive dirs.
**Fix:** Widen to `name[1024]` (NTFS-3G uses this), or return
variable-length with a caller-owned buffer.

#### Junctions / mount points / WOF files mis-reported as symlinks
**Location:** `src/lib.rs:309-320`
**Spec:** `FILE_ATTRIBUTE_REPARSE_POINT` only says "some reparse point".
The *tag* decides semantics
([MS-FSCC 2.1.2 Reparse Tags](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-fscc/c8e77b37-3909-4fe6-a4ea-2b9d423b1ee4)):
only `IO_REPARSE_TAG_SYMLINK` (0xA000000C) is a POSIX-style symlink.
Junctions (`IO_REPARSE_TAG_MOUNT_POINT` 0xA0000003), WOF (0x80000017),
LX symlinks, AppExecLinks, OneDrive placeholders, dedup reparses are
all distinct.
**Code:** If `$FILE_NAME.file_attributes` has the reparse bit, forces
`file_type = FS_NTFS_FT_SYMLINK` and `mode = 0o120777` without
inspecting `$REPARSE_POINT` at all.
**Why it matters:** Every junction on Windows (`C:\Users\All Users` →
`C:\ProgramData`, legacy `Documents and Settings`), every WOF-compressed
system binary, every OneDrive cloud placeholder reports as a symlink.
POSIX callers see `S_IFLNK`, call `readlink`, get ENOSYS, treat as
broken. `find(1)`/`fts(3)` follow-vs-not-follow behavior flips.
**Fix:** Read the `$REPARSE_POINT` 32-bit tag;
only map `IO_REPARSE_TAG_SYMLINK` → `FS_NTFS_FT_SYMLINK`.
Junctions → transparent directory or a new `FS_NTFS_FT_JUNCTION`.
Everything else → the underlying file/dir type.

#### Path traversal: `..` and `.` looked up literally
**Location:** `src/lib.rs:212-228`
**Spec:** NTFS `$INDEX_ALLOCATION` does **not** store `.` / `..`
entries ([Flatcap Concepts — Directory](https://flatcap.github.io/linux-ntfs/ntfs/concepts/directory.html));
parent comes from `$FILE_NAME.parent_directory_reference`. POSIX
drivers are expected to synthesize.
**Code:** `for component in path.split('/')` hands every component to
`NtfsFileNameIndex::find`. `..` and `.` are never in the index, so the
lookup returns "not found".
**Why it matters:** Any benign normalized path (`/a/./b`, `/a/../b`)
gets spurious ENOENT even when the target exists. rsync/diff clients
that hand us composed paths fail. The fact that `../../secret`
*happens* to miss is luck, not defense.
**Fix:** Skip `.` components; for `..`, walk to parent via the current
file's `$FILE_NAME.parent_directory_reference`.

#### Seek-by-reading in `fs_ntfs_read_file`
**Location:** `src/lib.rs:672-687`
**Spec:** Upstream `NtfsAttributeValue` implements `NtfsReadSeek` with
a real O(n_runs) `seek()`
([docs.rs/ntfs NtfsAttributeValue](https://docs.rs/ntfs/0.4/ntfs/attribute_value/enum.NtfsAttributeValue.html)).
**Code:** Skips `offset` bytes by looping `data_value.read` into an
8 KiB throwaway buffer.
**Why it matters:** pread at a 1 GiB offset on a media file does
~131 000 iterations, each doing attribute-value I/O (and full LZNT1
decompression for compressed $DATA), just to discard output. Turns
O(1) pread into O(offset); makes the bridge unusable for
random-access workloads (video scrubbing, VM disk images, DB files).
**Fix:** `data_value.seek(&mut reader, SeekFrom::Start(offset))` via
`NtfsReadSeek`, then read.

### Correctness risks

#### `NtfsFileNameIndex::find` matches DOS 8.3 names
**Location:** `src/lib.rs:221-224`
**Spec:** A file with a DOS shortname has two `$FILE_NAME` entries
— one per namespace
([Flatcap Filename Namespaces](https://flatcap.github.io/linux-ntfs/ntfs/concepts/filename_namespace.html)).
`find()` does not filter by namespace.
**Code:** Accepts whatever namespace matches.
**Why it matters:** `/PROGRA~1/...` resolves to `/Program Files/...`.
Usually benign but: (a) security-sensitive callers relying on canonical
paths are surprised; (b) short names are per-directory generated and
non-stable — caching one in a URL persists an unstable identifier;
(c) volumes with 8.3 generation disabled behave differently.
**Fix:** After `find()`, verify `$FILE_NAME.namespace()` ∈ {Win32,
Win32AndDos, Posix}; reject `Dos` unless opted-in.

#### Per-directory case-sensitivity flag ignored
**Location:** `src/lib.rs:196-232`
**Spec:** Since Windows 10 1803, individual directories can be flagged
case-sensitive
([MS case-sensitivity](https://learn.microsoft.com/en-us/windows/wsl/case-sensitivity)).
WSL and Docker-Desktop on Windows rely on it.
**Code:** Always case-insensitive (upcase table).
**Why it matters:** On a dev volume with case-sensitive directories,
`foo.txt` and `FOO.TXT` are distinct files. Our driver collapses them
to whichever the B-tree returns first. Listing shows both; lookup by
path is ambiguous.
**Fix:** Check the directory's flags; switch to byte-exact comparison
when the case-sensitive bit is set.

#### Files with only named streams read as error
**Location:** `src/lib.rs:641-651`, `include/fs_ntfs.h:133-134`
**Spec:** A file can have zero or more `$DATA` attributes; the unnamed
one is conventionally "the data" but isn't required
([Flatcap $DATA](https://flatcap.github.io/linux-ntfs/ntfs/attributes/data.html)).
Some MAPI/Outlook artifacts, WOF-compressed files, and encryption
placeholders have only named streams.
**Code:** Hard-requires the unnamed `$DATA`; errors if absent. `attr.size`
also stays 0 in that case, contradicting Windows Explorer.
**Why it matters:** Unreadable for an increasing class of modern Windows
files; size field lies.
**Fix:** If unnamed absent, try well-known fallbacks (WOF's
`WofCompressedData`). Expose `fs_ntfs_read_stream(path, stream_name, …)`
so callers can target the named stream they want.

#### WOF (Windows Overlay Filter) compression not supported
**Location:** `src/lib.rs:614-702` (entire read path)
**Spec:** Compact OS / per-file WOF encodes data in an ADS
`WofCompressedData`, compressed with XPRESS4K/8K/16K or LZX
([MS-XCA](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-xca/a8b7cb0a-92a6-4187-a23b-5e14273b96f8)).
The visible unnamed `$DATA` is *empty* + sparse and carries
`IO_REPARSE_TAG_WOF`. Upstream `ntfs` 0.4 handles only classic LZNT1
compression via `$DATA`'s compression_unit.
**Code:** Reads the (empty) unnamed `$DATA`, returns 0 bytes / garbage.
**Why it matters:** On any modern Win10/11 install a large fraction of
`C:\Windows\` is WOF-compressed. `notepad.exe`, `explorer.exe`, system
DLLs read as empty. **Silent data loss on every modern volume.**
**Fix:** Detect `IO_REPARSE_TAG_WOF`, parse the reparse data
(`FILE_PROVIDER_EXTERNAL_INFO_V1`), decompress `WofCompressedData`
ADS using a crate like `ms-compress`. Non-trivial but required for
correctness on modern volumes.

#### Eager directory materialization — unbounded memory
**Location:** `src/lib.rs:545-582`
**Spec:** `$INDEX_ALLOCATION` B+ trees support millions of entries;
`C:\Windows\WinSxS` routinely has 100k+.
**Code:** `dir_open` walks the whole index into `Vec<FsNtfsDirent>`.
~267 bytes per entry; 1M entries ≈ 270 MB per open dir. Malicious
images → OOM.
**Why it matters:** FSKit enumerates lazily; eager materialization
blocks the first `readdir` by seconds, defeats that, and visibly stalls
Finder on large dirs.
**Fix:** Keep the `NtfsIndexEntries` iterator alive in `FsNtfsDirIter`
and advance on `dir_next` (some lifetime plumbing required).

#### Dirty-volume state not surfaced; no MFT-mirror cross-check
**Location:** `src/lib.rs:357-372`, `src/lib.rs:392-407`
**Spec:** `$Volume`'s `$VOLUME_INFORMATION.flags` has `VOLUME_IS_DIRTY`
(0x0001). Clients SHOULD cross-check `$MFT` against `$MFTMirr` (first 4
entries) on mount
([Flatcap $MFTMirr](https://flatcap.github.io/linux-ntfs/ntfs/files/mftmirr.html)).
**Code:** Parses boot sector + upcase table; no dirty check, no mirror
cross-check, no `$LogFile` replay.
**Why it matters:** On a laptop force-rebooted / suspended, the MFT can
hold half-committed transactions. A read of a file mid-rename can
return the old name with the new file reference — correct-looking
path, wrong contents. A read-only driver can't replay, but it must at
minimum surface the state.
**Fix:** Expose `fs_ntfs_volume_info_t::is_dirty: uint8_t`. Read
`$Volume`/`$VOLUME_INFORMATION.flags`. Optionally add
`metadata_consistent` after comparing the first 4 MFT entries with
their mirror.

#### `unsafe impl Send for CallbackReader` claims more than it can guarantee
**Location:** `src/lib.rs:50-52`
**Spec:** `Send` means safe to transfer ownership across threads.
FSKit serializes *callbacks* per volume — that guarantees mutex-free
aliasing, not thread-binding of the handle.
**Code:** Unconditional `unsafe impl Send`.
**Why it matters:** If the Swift caller's `context` refers to a
thread-confined object (e.g. an `FSBlockDeviceResource` that enforces
main-thread drop), transferring drop to another thread violates that.
Papered-over unsafety; unlikely to bite in practice but worth tightening.
**Fix:** Document the Send-safety contract the Swift caller must uphold
for their `context` pointer; consider a `fs_ntfs_drop_on_thread(handle)`
helper for confined contexts.

### Incomplete coverage

#### No POSIX `mode` derivation from `$SECURITY_DESCRIPTOR`
**Location:** `src/lib.rs:272-278`
**Spec:** NTFS stores a Windows NT security descriptor (owner SID,
group SID, DACL, SACL) in `$SECURITY_DESCRIPTOR` / `$Secure:$SDS`
([Flatcap $SECURITY_DESCRIPTOR](https://flatcap.github.io/linux-ntfs/ntfs/attributes/security_descriptor.html));
ntfs-3g derives POSIX mode by mapping the DACL against a `UserMapping`
file.
**Code:** Hard-codes `0o40755` / `0o100644`.
**Why it matters:** Every file reports identical perms — no way to
surface read-only, executable, or denied-access. Even
`FILE_ATTRIBUTE_READONLY`, which *is* in `attributes`, isn't folded
into `mode`.
**Fix minimal:** Drop write bits when `FILE_ATTRIBUTE_READONLY` is set;
OR-in executable bits for common extensions (`.exe`, `.bat`, `.cmd`,
`.com`, `.ps1`).
**Fix full:** Parse `$SECURITY_DESCRIPTOR` + user-mapping config.

#### `fs_ntfs_readlink` stub — no reparse-point readback at all
**Location:** `src/lib.rs:708-717`
**Spec:** `$REPARSE_POINT` stores tag + data.
`IO_REPARSE_TAG_SYMLINK` → `SymbolicLinkReparseBuffer`
([MS-FSCC 2.1.2.4](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-fscc/b41f1cbf-10df-4a47-98d4-1c52a833d913))
with `PrintName` (display) and `SubstituteName` (NT form,
`\??\C:\path`).
`IO_REPARSE_TAG_MOUNT_POINT` → `MountPointReparseBuffer`
([MS-FSCC 2.1.2.5](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-fscc/ca069dad-ed16-42aa-b057-b6b207f447cc)).
**Code:** Returns -1 with "not yet implemented".
**Why it matters:** Every user profile has junctions (`Application
Data`, `My Documents`) a POSIX tool can't resolve. Combined with the
mis-typing bug above, every such entry is an unresolvable broken
symlink to callers.
**Fix:** Read `$REPARSE_POINT`; first 4 bytes = tag; decode `PrintName`;
strip `\??\` prefix; translate `C:\...` → `/...` for the caller.

#### No Unicode-normalization contract documented
**Location:** `src/lib.rs:236-261`, `src/lib.rs:565`, `include/fs_ntfs.h` (silent)
**Spec:** NTFS stores raw UTF-16; Windows compares via upcase table
(no normalization). `café` (NFC, 5 units) and `café` (NFD, 6 units)
are distinct on NTFS.
**Code:** Passes UTF-8 through unchanged. Correct, but undocumented.
**Why it matters:** darwin HFS+/APFS callers routinely normalize to
NFD before handing paths to FS APIs. Lookup fails with ENOENT on
otherwise-correct paths.
**Fix:** Document in `fs_ntfs.h` that paths use raw UTF-8 after
upcase-folding, and that callers must pre-normalize to whatever form
the file was stored as (which they cannot know). Long-term: accept a
normalization-mode flag at mount.

#### `.` and `..` not synthesized in directory listings
**Location:** `src/lib.rs:545-582`
**Spec:** POSIX `readdir(3)` expects `.` and `..`; NTFS indexes don't
store them.
**Code:** Returns neither.
**Why it matters:** POSIX consumers (FSKit's BSD-derived enumeration,
`fts(3)` ports) that rely on entry count / presence break silently.
**Fix:** Prepend synthesized entries in `dir_open`:
`{name=".", frn=dir.frn}`, `{name="..", frn=parent.frn}` where parent
comes from the dir's `$FILE_NAME.parent_directory_reference`.

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
