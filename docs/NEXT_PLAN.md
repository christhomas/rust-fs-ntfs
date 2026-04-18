# fs-ntfs — what's next after W0→W4

The W-plan shipped the driver's basic read/write surface: recovery,
in-place writes, truncate/grow, full CRUD (create/unlink/mkdir/rmdir/
rename), ADS, reparse points, EAs. That's the "80% of ntfs-3g
semantics at maybe 20% of its LOC" outcome the original plan
targeted. What's missing breaks down into six buckets.

## Priority legend

- 🔴 **Correctness** — known wrong-behavior paths (some are in
  `docs/STATUS.md` §Documentation cross-check; some we introduced
  during W-plan work).
- 🟠 **Scale** — works on small fixtures but breaks down on realistic
  volumes.
- 🟡 **Completeness** — features the spec has that we don't.
- 🟢 **Polish** — won't corrupt anything, but makes the crate nicer.
- 🔵 **Tooling** — things around the crate rather than in it.

---

## 1. 🔴 Correctness — the real hazards

These are bugs I'd rank above any feature work. All are documented
in `docs/STATUS.md`'s cross-check section except where noted.

### 1.1 NTFS upcase-table collation

**Today**: `index_io::compare_utf16_case_insensitive` does ASCII-only
case-folding. We use it for B+ tree insertion sort order.

**Problem**: For non-ASCII filenames (`café`, `日本`, etc.), our
insertion sort differs from the NTFS upcase-table collation that
Windows expects. Upstream's binary search via `NtfsFileNameIndex::find`
then fails to locate entries it should find. Silent data-loss path.

**Fix**: load `$UpCase` from the volume (MFT record 10), map each
UTF-16 unit through it, compare folded units. ~150 LOC. Tests:
create files with CJK / emoji names, verify upstream finds them.

### 1.2 Reparse-tag dispatch on read

**Today**: `fill_attr` in `src/lib.rs` marks any file with the
REPARSE_POINT flag as a symlink (`FS_NTFS_FT_SYMLINK`, mode
`0o120777`). Any reparse file reads as symlink.

**Problem**: Junctions, WOF-compressed files, OneDrive placeholders,
LX symlinks all mis-typed. Call `readlink()` on a junction, get
ENOSYS; POSIX tools get confused.

**Fix**: read the `$REPARSE_POINT`'s 32-bit tag on stat; map
`IO_REPARSE_TAG_SYMLINK` (0xA000000C) only to symlink; map
`IO_REPARSE_TAG_MOUNT_POINT` (0xA0000003) to a new
`FS_NTFS_FT_JUNCTION` (or transparently follow); other tags fall
through as directory/file. ~50 LOC. Pairs with the new
`fs_ntfs_readlink` implementation that actually decodes the
`SymbolicLinkReparseBuffer` / `MountPointReparseBuffer`.

### 1.3 Timestamp widening to 64-bit + nanoseconds

**Today**: `FsNtfsAttr::atime`/`mtime`/`ctime`/`crtime` are `uint32_t`
UNIX-epoch seconds. `ntfs_time_to_unix` does
`.saturating_sub(EPOCH_DIFF) as u32`.

**Problem**: pre-1970 timestamps clamp to 0, post-2038 timestamps
wrap, sub-second precision is dropped. Silently wrong for backup
metadata, SMB peers, archive volumes.

**Fix**: widen to `int64_t seconds + uint32_t nsec`. Breaks the C
ABI — bundle with any other planned ABI breaks. ~50 LOC.

### 1.4 `fs_ntfs_dirent_t::name[256]` truncation

**Today**: fixed 256-byte buffer, `min(name_bytes.len(), 255)`.

**Problem**: NTFS filenames are 255 UTF-16 code units. UTF-8
encoding can be up to 4 bytes/unit → 1020 bytes max. A single
emoji or long CJK name gets silently truncated; subsequent
`fs_ntfs_stat` on the truncated name fails with ENOENT.

**Fix**: widen to `name[1024]` (ABI break, bundle). ~5 LOC.

### 1.5 Seek-by-reading in `fs_ntfs_read_file`

**Today**: skips `offset` bytes by `data_value.read()`-ing into a
throwaway 8 KiB buffer in a loop.

**Problem**: O(offset). A 4 GiB pread on a VM disk image runs
524288 iterations, each decompressing (for LZNT1) or decoding
(for sparse/non-resident).

**Fix**: `data_value.seek(reader, SeekFrom::Start(offset))` via
upstream's `NtfsReadSeek` trait. ~10 LOC.

### 1.6 Path traversal — `.` and `..` components

**Today**: `navigate_to_path` in `src/lib.rs` splits on `/` and
blindly calls `NtfsFileNameIndex::find` on every component. `..`
and `.` are never in the index so they always miss.

**Problem**: `/a/./b` and `/a/../b` return ENOENT even when `/b`
exists. Rsync/diff-style clients composing paths with them fail.

**Fix**: skip `.`; for `..` walk to parent via `$FILE_NAME
parent_directory_reference`. ~30 LOC.

### 1.7 `.`/`..` in directory listings

**Today**: `fs_ntfs_dir_open` / `_next` yield only real entries.
NTFS indexes don't store `.` / `..`.

**Problem**: POSIX `readdir` callers expect them. Breaks `fts(3)`
ports, some Finder behaviors, most `find -type d` invocations.

**Fix**: synthesize two entries at the start of iteration.
~30 LOC.

### 1.8 Proper NTFS collation in our own index-root insert

**Today**: our `insert_entry_into_index_root` uses the same naive
ASCII comparator. A file created with a non-ASCII name lands at
the wrong sort position, so Windows' binary search misses it.

**Fix**: same upcase-table work as 1.1.

---

## 2. 🟠 Scale — limits that break on real volumes

### 2.1 MFT self-growth

**Today**: `create_file` / `mkdir` fail with "MFT full" once the
resident (or initial non-resident) `$MFT:$Bitmap` has no free
bits. Small volumes hit this at a few hundred files.

**Fix**: implement `$MFT` growth:
- Find free clusters via volume bitmap.
- Extend `$MFT`'s `$DATA` runs to cover them.
- Extend `$MFT:$Bitmap` by enough bits to cover the new records.
- The extension of `$MFT`'s own bitmap is the recursion problem:
  use scratch space in an existing MFT record to hold the bitmap
  byte you're about to flip, then commit.

~300 LOC. This is the gatekeeper for "infinite creates" scale.

### 2.2 B+ tree split on INDX-block full

**Today**: `insert_entry_in_parent` errors out when every INDX
block in a parent directory is full.

**Fix**: allocate a new cluster for `$INDEX_ALLOCATION`, add a VCN
to the `$I30` bitmap, pick a split point, move half the entries
into the new block, insert a subnode pointer into the parent
level. If the root-level `$INDEX_ROOT` splits, promote it to
`$INDEX_ALLOCATION`. ~500 LOC, the hardest single piece of
remaining write work.

### 2.3 Non-resident attribute promotion beyond `$DATA`

**Today**: `$REPARSE_POINT`, `$EA`, named `$DATA` all refuse to
grow past resident capacity.

**Fix**: generalize `promote_resident_data_to_nonresident` to
handle any attribute type. The mechanics are identical; the
guarded behavior (which attribute types make sense non-resident,
what compression/sparse flags are valid) differs. ~150 LOC.

### 2.4 Large-volume boot-sector paths

**Today**: all fixtures are 16–64 MiB. Cluster-size 4 KiB with
512-byte sectors. Well-tested only for this exact shape.

**Fix**: fixture matrix — add 512-MiB + 2-GiB fixtures, cluster
sizes 512 / 4096 / 65536, sector sizes 512 / 4096 (Advanced
Format). Probably catches 2–3 subtle off-by-ones. ~200 LOC of
_vm-builder changes plus fresh tests.

### 2.5 Dirent eager materialization still present

**Today**: `fs_ntfs_dir_open` reads every entry into a Vec.

**Problem**: 270 MB on a 1M-entry directory. FSKit OOMs.

**Fix**: lazy iterator holding upstream's `NtfsIndexEntries` +
reader reference. Lifetime plumbing is awkward but bounded. ~100
LOC.

---

## 3. 🟡 Completeness — spec features still missing

### 3.1 Hard links

`$FILE_NAME` supports multiple entries (one per hard link). We
synthesize exactly one. New API:
`fs_ntfs_link(image, existing_path, new_path)` → increment
link_count, add a new `$FILE_NAME`, add a new index entry in the
target parent.

### 3.2 NTFS compression (LZNT1)

Both directions:
- **Read**: already works via upstream. Good.
- **Write**: we refuse anything with the compression flag set.
  Writing new compressed data means emitting LZNT1-encoded chunks
  per `compression_unit`. ~800 LOC — big.

### 3.3 Sparse-file explicit management

POSIX `fallocate(FALLOC_FL_PUNCH_HOLE)`-style:
`fs_ntfs_punch_hole(image, path, offset, len)` → mark range as
sparse in the data runs, free the clusters. Our current truncate
can free tail clusters; hole-punching frees middle clusters.

### 3.4 `$SECURITY_DESCRIPTOR` writes

`$Secure` / `$SDS` lookup is a separate rabbit hole (stream of
SIDs + ACEs, shared across files by security_id in SI). Minimal
version: let the caller OR bits into SI's `file_attributes`
(we already do READONLY via `chattr`); punt on full ACL support.

### 3.5 `$OBJECT_ID`

16-byte GUID per file, used by DLT (Distributed Link Tracking).
Rarely needed, but some Windows APIs inspect it. A few lines of
attribute-builder code.

### 3.6 Proper `readlink` with reparse-tag dispatch

Paired with §1.2. Decode `SymbolicLinkReparseBuffer` /
`MountPointReparseBuffer`; strip the `\??\` NT-path prefix;
translate `C:\...` → `/...` for POSIX callers. ~100 LOC.

### 3.7 Non-resident named streams + EAs

For the rare but possible case of a multi-MiB alternate data
stream or a huge EA payload. Same mechanics as §2.3.

### 3.8 WOF (Windows Overlay Filter) decompression

Modern Windows 10/11 volumes have most of `C:\Windows\` stored as
empty unnamed `$DATA` + `IO_REPARSE_TAG_WOF` + `WofCompressedData`
ADS. Without WOF support, reading `notepad.exe` returns 0 bytes.
Requires XPRESS4K/8K/16K + LZX decompression. Third-party
crate (`ms-compress`) does this; bindings would be ~200 LOC,
decompressor work is what it is (the crate does it for us).

### 3.9 Case-sensitive directory flag

`FILE_CASE_SENSITIVE_DIR` (WSL / Docker-Desktop). Our writes
never set it; our reads never check it. On a dev machine with
case-sensitive subdirs, we collapse `foo.txt` and `FOO.TXT` to
whichever the B-tree finds first.

---

## 4. 🟢 Polish — small but user-visible

### 4.1 `fs_ntfs_last_errno()` companion

Parallel to `fs_ntfs_last_error()` the string. Returns POSIX
errno (ENOENT / EIO / EINVAL / ENOSPC / ...). Lets FFI callers
do proper error-code dispatching instead of parsing error strings.

### 4.2 Rust-native facade

`Filesystem::mount(path) -> Result<Filesystem>` / `Filesystem::open(path)`
/ `File::read_at` / `File::write_at`. Just idiomatic wrappers
around the C-ABI raw pointers. ~200 LOC. Makes Rust consumers
much happier.

### 4.3 Structured volume statistics

`fs_ntfs_volume_info_t` currently has total/cluster_size/serial.
Add free-cluster count, MFT free-record count, index
fragmentation hints, dirty flag. ~50 LOC.

### 4.4 `fs_ntfs_is_dirty(image)` standalone

Light-weight "just peek at the flag" without full mount. Useful
for mount-time precondition checks. ~20 LOC.

### 4.5 Wire up `best_file_name_str`

Dead-code-allow in `src/lib.rs` for months. Use it in
`fill_attr` + dir iterator to return Win32-namespace names
preferentially, deduplicating the DOS namespace fallback.

---

## 5. 🔵 Tooling — around the crate

### 5.1 `capi_*` test suite completion

Only `capi_fsck`, `capi_write_w1`, `capi_write_content`,
`capi_write_truncate` today. The W3/W4 C-ABI functions
(`fs_ntfs_create_file`, `_mkdir`, `_rename`, `_unlink`,
`_write_named_stream`, `_write_reparse_point`, `_create_symlink`,
`_write_ea`, `_remove_ea`, etc.) are only tested via the Rust
helpers. The *exact* behavior the FSKit consumer sees (CString
lifetimes, out-pointer writes, null-pointer handling) is
untested for those. ~400 LOC of tests.

### 5.2 Fuzz harness

`cargo-fuzz` target for `data_runs::decode_runs`,
`ea_io::decode`, `attr_io::iter_attributes`. All three take raw
bytes and are the most likely panic sources on a crafted image.
Finds off-by-ones fast.

### 5.3 Criterion benchmarks

`bench/write_at_1gb.rs`, `bench/create_many.rs`. Detects
performance regressions as we refactor. Especially useful once
§1.5 lands — confirm pread went from O(offset) to O(log n).

### 5.4 CI matrix expansion

Today: Ubuntu-latest only. Add macOS-latest (important — ntfs-3g
binary compat is different), Windows (cross-compile test). Rust
versions: stable + MSRV.

### 5.5 Sanitizer runs

`cargo +nightly test -Zsanitizer=address`. Our raw-byte buffer
manipulations are the most likely spot for OOB reads/writes.

### 5.6 Clippy gate

Already flagged: pre-existing FFI `not_unsafe_ptr_arg_deref`
lints. Requires either marking FFI `unsafe extern "C"` (ABI-
visible change) or explicit `#[allow]` at the boundary. Until
it's done, CI runs `cargo clippy` against nothing — regressions
land uncaught.

### 5.7 Release pipeline

Tag-driven publication of `libfs_ntfs-vX.Y.Z-macos-universal.tar.gz`
cloned from fs-ext4 — exists but never verified.

### 5.8 `cargo-deny` / licence hygiene

Since we're strict about no-GPL, a `cargo-deny` step in CI that
asserts the dependency licenses stays our-license-compatible
(MIT/Apache/BSD) would catch accidental regressions.

---

## 6. 🧠 Observability + safety — invisible until they're not

### 6.1 Transactional semantics across multiple records

`create_file` touches: MFT record for the new file + parent
record + `$MFT:$Bitmap`. Each is individually fsync'd, but
there's no multi-record atomicity. A crash mid-create can leave:
- MFT record populated, bitmap bit set, no index entry — leaked
  allocation (space wasted, no correctness issue).
- MFT record populated, no bitmap bit — allocator may reuse the
  slot, overwriting the record.

**Fix options**:
- Stricter ordering: MFT record → bitmap bit → index entry. A
  crash at any point leaks at worst the MFT record allocation.
  We already mostly do this.
- Intention log: write a tiny "I'm about to X" record in a
  dedicated scratch attribute, replay on mount. Essentially
  mini-journal. ~500 LOC.

### 6.2 `$LogFile` writeback + replay

The full-journaling answer. Multi-month project; out of scope
per original W5 decision. Worth revisiting if we find a
customer with strict durability requirements.

### 6.3 Volume-scale corruption tests

Fuzz with *mutations* rather than raw bytes: flip random bits
in a real NTFS image, attempt mount. Do we panic? Return
clean errors? Accept corrupt state? The answer should be
"clean error, never panic." Today: unknown.

### 6.4 Tracing hooks

A `tracing` subscriber or `log` call at attribute read / cluster
alloc / bitmap flip so consumers can instrument real-world
usage. Particularly useful for debugging FSKit reports.

---

## Sequencing

Suggested order for a follow-on quarter:

1. **Immediate correctness batch** (~1–2 weeks): §1.1 upcase,
   §1.2 reparse-tag dispatch, §1.3+1.4 ABI widening (bundled as
   one breaking change), §1.5 seek, §1.6+1.7 dot-components.
   Closes the most-obvious read-side bugs.
2. **`capi_*` completion** (§5.1, ~3 days): every C-ABI function
   gets a dedicated test. Locks behavior for FSKit consumers.
3. **MFT growth + B+ tree split** (§2.1 + §2.2, ~3 weeks):
   unlocks realistic scale. This is the single biggest engineering
   item remaining.
4. **Rust-native facade + errno companion** (§4.1 + §4.2,
   ~1 week): makes the crate pleasant to consume from Rust.
5. **WOF decompression** (§3.8, ~2 weeks): closes the modern-
   Windows read-correctness gap from STATUS.md.
6. **Hard links, proper readlink, large-volume fixtures** (§3.1,
   §3.6, §2.4, ~1–2 weeks combined): completeness polish.

Beyond that, §6 (observability + journaling) is where a
"production-ready NTFS driver" graduates from "good enough for
an FSKit extension" to "ready for high-availability use". That's
a separate project, easily a person-year.

---

## What stays out of scope

- **Write support for encrypted files** (EFS). Requires
  Windows-only crypto stack. Refuse.
- **Upstream `ntfs` 0.5+ migration**. Keep pinned at 0.4 until
  upstream changes force otherwise.
- **Quota management**. Rare in practice; refuse for now.
- **32-bit target support**. FSKit is 64-bit only.

---

_Last updated: concurrent with the W-plan wrap-up. Revisit when
a consumer asks for anything in §6 or §3.8._
