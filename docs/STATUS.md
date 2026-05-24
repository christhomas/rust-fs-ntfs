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
| `fs_ntfs_get_volume_info(fs, out)` | Label, cluster size, total/used clusters, version (v1 shape). |
| `fs_ntfs_get_volume_info_v2(fs, out)` | v2 extension: adds `volume_flags`, `is_dirty`, `mft_record_size`, `bytes_per_sector`. v1 fields stay at identical offsets. |
| `fs_ntfs_stat(fs, path, out)` | Per-file metadata — size, timestamps, file type. |
| `fs_ntfs_dir_open/next/close(fs, path)` | Streaming directory iterator. One entry at a time; `_next` returns NULL when exhausted. |
| `fs_ntfs_read_file(fs, path, offset, length, buf)` | Pread-style data read. Returns bytes read or -1 on error. |
| `fs_ntfs_readlink(fs, path, buf, cap)` | Reparse-point / symlink target. See limitations below. |
| `fs_ntfs_read_reparse_point(image, path, &tag, buf, len, &total)` | Raw `(reparse_tag, data)` payload for any reparse type. Complements `readlink` (symlink/mount-point only). |
| `fs_ntfs_read_object_id(image, path, out_buf)` | Read the 16-byte `$OBJECT_ID` GUID when present. |
| `fs_ntfs_read_object_id_extended(image, path, out_buf, len)` | Read the 64-byte form (object_id + 3 Birth GUIDs, MS-FSCC §2.4.6). |
| `fs_ntfs_read_security_id(image, path, &out)` | `$STANDARD_INFORMATION.security_id`. Returns 1 (id), 0 (48-byte v1.x form — no field), -1 (error). |
| `fs_ntfs_read_si_full(image, path, &out)` | Every MS-FSCC §2.4.2 SI field — timestamps, attributes, plus the optional NTFS 3.x trailer (owner_id, security_id, quota, usn) flagged by `has_v3`. |
| `fs_ntfs_read_volume_label(image, buf, len)` | `$VOLUME_NAME` decoded to UTF-8. Empty result = no label. |
| `fs_ntfs_list_named_streams(image, path, buf, len, &total)` | Names of every named `$DATA` attribute (ADS), excluding the unnamed primary. NUL-separated, size-queryable. |
| `fs_ntfs_list_ea_keys(image, path, buf, len, &total)` | EA names only (skips values up to 64KB each — cheap enumeration). NUL-separated, size-queryable. |
| `fs_ntfs_free_clusters(image)` / `fs_ntfs_mft_free_records(image)` | Volume-stat probes. |

### Writes

All path-based ops below also have a handle-based `_h` sibling
(e.g. `fs_ntfs_create_file_h`) that operates against an open
`FsNtfsHandle` so the volume stays mounted through the callback
adapter. Outstanding limits — overflowed-index parents and a full
MFT — are tracked in [`FUTURE_FEATURES.md`](FUTURE_FEATURES.md).

| Function | Role |
|---|---|
| `fs_ntfs_set_times(image, path, …)` | Patch the four NT timestamps in `$STANDARD_INFORMATION`. Per-field nullable; only updates SI, not the parent index `$FILE_NAME` copy (matches Windows). |
| `fs_ntfs_set_file_attributes(image, path, add, remove)` | Flip `FILE_ATTRIBUTE_*` bits per MS-FSCC 2.6. `add`/`remove` overlap is rejected. |
| `fs_ntfs_write_file(image, path, offset, buf, len)` | In-place write into existing non-resident `$DATA`. Refuses extension past EOF, compressed streams, sparse holes. |
| `fs_ntfs_write_resident_contents(image, path, data, len)` | Replace a file's resident `$DATA`. Refuses non-resident streams. |
| `fs_ntfs_write_file_contents(image, path, data, len)` | High-level dispatcher: tries resident, auto-promotes to non-resident on capacity error. |
| `fs_ntfs_truncate(image, path, new_size)` | Shrink/grow a non-resident `$DATA`. |
| `fs_ntfs_grow(image, path, new_size)` | Append clusters to a non-resident `$DATA`; new bytes read as zero. |
| `fs_ntfs_create_file(image, parent, basename)` | Synthesize a fresh MFT record + insert into parent's `$INDEX_ROOT`. |
| `fs_ntfs_mkdir(image, parent, basename)` | Same as `create_file` but emits an empty `$INDEX_ROOT:$I30` and no `$DATA`. |
| `fs_ntfs_unlink(image, path)` | Remove file: parent index entry + truncate-to-0 + IN_USE clear + `$MFT:$Bitmap` free. |
| `fs_ntfs_rmdir(image, path)` | Delete an empty directory. Refuses non-empty/overflowed dirs. |
| `fs_ntfs_rename(image, old_path, new_basename)` | Rename within current parent (same- or different-UTF-16-length). |
| `fs_ntfs_rename_same_length(image, old_path, new_basename)` | Fast path when UTF-16 length is unchanged — patches index entry + `$FILE_NAME` directly. |
| `fs_ntfs_link(image, existing, new_parent, new_basename)` | Hard link: extra `$FILE_NAME` + parent index insert + link-count bump. |
| `fs_ntfs_write_named_stream(image, path, stream_name, data)` | Upsert a named `$DATA` (ADS); promotes to non-resident when needed. |
| `fs_ntfs_delete_named_stream(image, path, stream_name)` | Remove a named `$DATA` attribute. |
| `fs_ntfs_write_reparse_point(image, path, tag, data)` | Upsert resident `$REPARSE_POINT` and set `FILE_ATTRIBUTE_REPARSE_POINT`. |
| `fs_ntfs_remove_reparse_point(image, path)` | Reverse of above. |
| `fs_ntfs_create_symlink(image, parent, basename, target, relative)` | `create_file` + reparse-point with `IO_REPARSE_TAG_SYMLINK`. |
| `fs_ntfs_write_ea(image, path, name, value)` | Upsert an extended attribute. |
| `fs_ntfs_remove_ea(image, path, name)` | Remove an extended attribute. |
| `fs_ntfs_write_object_id(image, path, in_buf)` | Write the 16-byte `$OBJECT_ID` GUID. Adds the attribute if absent, replaces in place if present. |
| `fs_ntfs_write_object_id_extended(image, path, in, bv, bo, bd)` | Write the 64-byte extended form (object_id + 3 Birth GUIDs, MS-FSCC §2.4.6). `_h` sibling for handle-based callers. |
| `fs_ntfs_remove_object_id(image, path)` | Remove the `$OBJECT_ID` attribute. |
| `fs_ntfs_set_security_id(image, path, security_id)` | Point a file at an existing `$Secure:$SDS` entry (e.g. the mkfs system-files DACL at `0x100`). Requires the 72-byte NTFS 3.x SI form. Adding new SD entries is separate. |
| `fs_ntfs_set_volume_label(image, label)` | Rename the volume `$VOLUME_NAME`. Empty label removes the attribute. |

### Recovery / volume tools

| Function | Role |
|---|---|
| `fs_ntfs_is_dirty(image)` / `fs_ntfs_is_dirty_with_callbacks(cfg)` | Probe `VOLUME_IS_DIRTY` without modifying. |
| `fs_ntfs_clear_dirty(image)` | Clear `VOLUME_IS_DIRTY` in `$Volume/$VOLUME_INFORMATION`. Returns 1 (cleared), 0 (already clean), -1 (error). |
| `fs_ntfs_reset_logfile(image)` | Overwrite `$LogFile` with `0xFF` — the format-level "no pending transactions" signal. |
| `fs_ntfs_fsck(image, *logfile_bytes, *dirty_cleared)` | Both above; `NULL` out-params accepted. |
| `fs_ntfs_fsck_with_callbacks(cfg, …)` | Same as `fsck` but goes through the block-device callback adapter (sandbox-safe). |
| `fs_ntfs_mkfs(cfg)` | Format a volume from scratch via the callback adapter. |

### Data types

- `FsNtfsHandle` — opaque handle to a mounted filesystem.
- `FsNtfsAttr` — file metadata (size, timestamps, file_type, etc).
- `FsNtfsDirent` — one entry in a directory listing.
- `FsNtfsVolumeInfo` — volume-scoped info.
- `FsNtfsBlockdevCfg` — read callback + context + size for the callback mount path.
- `FsNtfsDirIter` — opaque directory iterator (heap-allocated, must be closed).

## Current matrix state

A 42-scenario Mac→Windows-VM test matrix exercises mkfs + mount +
chkdsk on real `ntfs.sys`. The matrix is sealed by binary hash via
`test-diagnostics/matrix-results.json` (`tested_at_sha` + `binary_sha256`
+ VM metadata + per-scenario verdict).

| Branch     | tested_at_sha | Wall time   | Scenarios passed | Source                                                                                |
| ---------- | ------------- | ----------- | ---------------- | ------------------------------------------------------------------------------------- |
| `staging`  | `30fcdd6`     | 11369 s     | 42 / 42          | PR #49 stacked features                                                               |
| `staging-2`| `d9595c7`     | 13794 s     | 42 / 42          | PR #49 + 5 staging-2 features (`read_reparse_point`, `list_named_streams`, …)         |

Verify a working tree against the committed seal:

```sh
bash scripts/matrix-verify.sh
# → "sealed by SHA (…)" or "sealed by binary content (…)"
```

Run the full matrix against a Windows VM (~3-4 hr; needs `.test-env`):

```sh
bash scripts/matrix-baseline.sh           # full 42-scenario sweep
bash scripts/matrix-baseline.sh --smoke   # 5 representative scenarios (~15 min)
```

All 42 scenarios reach `chkdsk readonly = 0` (no problems) and
`chkdsk /scan ∈ {0, 11, 13}` (per-scenario distribution recorded in
the JSON). The `/scan = 13` ceiling on fresh-format volumes is a
known still-open differentiator tracked in
[`docs/FUTURE_FEATURES.md` §3.1](FUTURE_FEATURES.md) — every other
verdict (mount, write, repeat-mount, repair) is clean. Detailed
findings live in
[`docs/chkdsk-improvement-findings.md`](chkdsk-improvement-findings.md)
and the per-bug catalog in
[`docs/mkfs-bug-catalog.md`](mkfs-bug-catalog.md). Spec-level rules
extracted from these investigations live in the spec sections
([§1 geometry](spec/sections/01-geometry-boot.md),
[§2 MFT records](spec/sections/02-mft-records.md),
[§4 indexes](spec/sections/04-indexes-directories.md),
[§6 special streams](spec/sections/06-special-streams.md)).

## Test infrastructure

Fixture-driven integration tests. Real NTFS images are generated inside
a qemu-hosted Alpine Linux VM (the only portable way to run
`NTFS formatter` / loop-mount NTFS on macOS or CI). `NTFS driver` is used **only
during fixture creation** — the cargo test binary opens the `.img`
files raw via the `ntfs` crate, no FUSE, no kernel driver.

Layout:

```
test-disks/
  build-ntfs-feature-images.sh   # host-side qemu orchestrator
  _vm-builder.sh                 # guest-side NTFS formatter + mount + populate
  .vm-cache/                     # Alpine ISO + kernel + apks (gitignored)
  ntfs-*.img                     # generated, gitignored
tests/
  common/mod.rs                  # shared open/navigate/read helpers
  integration.rs                 # basic fixture
  manyfiles.rs large_file.rs sparse.rs ads.rs unicode.rs deep.rs
.github/workflows/ci.yml         # installs qemu, runs the script, cargo test
```

Fixtures produced (all via `NTFS formatter` + `NTFS driver` mount, all MFT-backed
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

<!-- - **Writes of any kind.** All APIs are read-only. Upstream `ntfs` does
  not expose mutating operations at all in 0.4, so this isn't a small
  addition.
COMMENTED OUT: emphatically wrong now. The crate ships a full
write surface (mkfs + W0/W1/W2/W3) implemented locally without
waiting for upstream `ntfs`. See include/fs_ntfs.h "In-place
writes" / "Filesystem creation" sections and the Write-path test
table above. -->
- **Alternate Data Streams (ADS).** NTFS files can have named data
  streams (`file.txt:stream_name`). The read-file API only reads the
  default unnamed data attribute. An API extension along the lines of
  `fs_ntfs_read_stream(fs, path, stream_name, …)` would be the
  idiomatic way to surface this.
  <!-- Partial-stale: the read side of this is still accurate
  (no fs_ntfs_read_stream / fs_ntfs_list_streams). The WRITE side
  is now shipped — fs_ntfs_write_named_stream /
  fs_ntfs_delete_named_stream at include/fs_ntfs.h:407, 414. -->
<!-- - **Extended attributes / `$EA`.** Not surfaced.
COMMENTED OUT: write side shipped — fs_ntfs_write_ea /
fs_ntfs_remove_ea at include/fs_ntfs.h:364, 373. A read-side
fs_ntfs_read_ea / list_ea is still missing; track that under a new
limitation entry if it matters. -->
- **Per-file NTFS permissions / ACLs.** Stat returns a simplified POSIX
  mode; native NTFS ACLs are ignored.
<!-- - **Reparse points / symlink target resolution.** `fs_ntfs_readlink`
  exists but only handles trivial symlink reparse-point tags; junctions
  and mount points need specialised handling that isn't implemented.
COMMENTED OUT: shipped. src/lib.rs:985+ decodes both
SymbolicLinkReparseBuffer and MountPointReparseBuffer (junctions);
fill_attr at src/lib.rs:484-499 dispatches the tag correctly.
tests/readlink.rs covers it. WOF / LX / AppExecLink / dedup tags
still fall through (see Phase 3 #14 for WOF decompression). -->
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

<!-- - ~~`best_file_name_str` in `src/lib.rs` carries a drive-by
  `#[allow(dead_code)]`.~~ Deleted (§4.5). Dir iterator already
  skips DOS-namespace entries at the index level, so the function
  was unused. Files with only a DOS `$FILE_NAME` (exceedingly rare
  on disks created by modern tools) still show up with that name.
- ~~**Clippy gate disabled in CI.**~~ Resolved (§5.6): crate-level
  `#![allow(clippy::not_unsafe_ptr_arg_deref)]` on `lib.rs` unblocks
  the ABI without forcing `unsafe extern "C"` on every consumer.
  `cargo clippy --all-targets` now compiles; only cosmetic warnings
  remain (useless_format, manual_div_ceil, etc.) — enabling
  `-D warnings` is fine once those are cleaned up.
COMMENTED OUT: best_file_name_str already deleted (resolved); the
Clippy entry conflates two states — the crate-level `allow` is in
src/lib.rs:12 but `.github/workflows/ci.yml` still has the clippy
step commented out, so the CI gate is NOT actually re-enabled. The
real outstanding work lives in Phase 2 #7 below; keeping the
half-true "Resolved" claim here is misleading. -->
- `fs_ntfs_read_file` copies through an intermediate buffer; direct
  scatter-read into the caller's buffer is doable but wasn't
  implemented.
<!-- - Thread-local `last_error` is single-string; some consumers would
  benefit from a structured errno companion (`fs_ntfs_last_errno()`)
  matching the pattern in `fs-ext4`.
COMMENTED OUT: shipped. `fs_ntfs_last_errno()` is in
include/fs_ntfs.h:177 and src/lib.rs:132. tests/errno_companion.rs
covers it. -->

**Outstanding (Phase 2 #7 still open):** the CI clippy step is still
commented out in `.github/workflows/ci.yml` ("clippy deferred…
Re-enable once resolved"). The crate-level `#![allow(...)]` lines
in `src/lib.rs` are in place; what remains is flipping the CI step
back on with `-D warnings`.

## Test coverage

<!-- 193 integration tests across 31 test files. See
[Test infrastructure](#test-infrastructure) above for the fixture
layout; all 138 pass locally and in CI.
COMMENTED OUT: counts are stale and internally inconsistent (193 vs
138). `tests/` now has ~56 .rs files including capi_*, mkfs_*,
facade, errno_companion, corruption_fuzz, object_id, path_dots,
readdir_dots, readlink, upcase, volume_stats, write_link,
write_root_ops, write_ea, write_ads_promote, etc. that aren't
itemised in the tables below. Re-derive with `cargo test` before
quoting numbers. -->
See [Test infrastructure](#test-infrastructure) above for the
fixture layout. The Read- and Write-path tables below cover the
original suites; the post-W3 additions (capi_link, capi_remove,
capi_rename, capi_reparse, capi_create, capi_ea,
capi_fsck_callbacks, capi_write_variants, mkfs_roundtrip,
mkfs_bin_smoke, facade, errno_companion, corruption_fuzz,
object_id, path_dots, readdir_dots, readlink, upcase, volume_stats,
write_link, write_root_ops, write_ea, write_ads_promote, …) are
not yet itemised.

**Read path** (43 tests, unchanged from the initial testsuite work):

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

**Write path** (95 tests added across W1/W2/W3):

| File | Fixture | Tests | What's covered |
|---|---|---|---|
| `mft_io.rs` | ntfs-basic | 9 | USA fixup round-trip, boot-params parse, RMW identity, torn-write detection, free-record refusal. |
| `write_times.rs` | ntfs-basic | 6 | `set_times` all-four, partial field writes, nested path, missing-path rejection, remount round-trip, upstream post-write mount. |
| `write_attrs.rs` | ntfs-basic | 6 | `set_file_attributes` add/remove/multi/overlap/survive-remount/isolation. |
| `capi_write_w1.rs` | ntfs-basic | 6 | C-ABI `fs_ntfs_set_times` / `fs_ntfs_file attribute tools` including null-path + overlap. |
| `write_content.rs` | ntfs-large-file | 7 | `write_at` non-resident rewrite: MB boundary, surrounding bytes, past-EOF rejection, resident rejection, zero-len no-op, remount, cluster-boundary spanning. |
| `capi_write_content.rs` | ntfs-large-file | 4 | C-ABI `fs_ntfs_write_file` happy path + error cases. |
| `data_runs.rs` | — | 20 | Mapping-pair decoder + encoder round-trips: sparse, negative delta, multi-byte LCN, VCN-gap rejection, edge cases. |
| `bitmap.rs` | ntfs-basic | 10 | `$Bitmap` allocator: LCN 0/MFT allocated, find-free-run, alloc/free round-trip, double-alloc rejected, double-free rejected, overflow bounds, survives-remount. |
| `write_truncate.rs` | ntfs-large-file | 7 | Shrink to half/zero/same/partial cluster; bitmap clusters freed; grow rejected; upstream reads truncated file. |
| `capi_write_truncate.rs` | ntfs-large-file | 4 | C-ABI `fs_ntfs_truncate` including grow rejection + null-path. |
| `write_grow.rs` | ntfs-large-file | 5 | Append clusters, new bytes read as zero, bitmap free count drops, shrink rejected, remount. |
| `attr_resize.rs` | ntfs-basic | 8 | Resident attribute resize via volume-label rename: same-len / shrink / grow / zero-len / round-trip / huge-grow rejection / preserves-subsequent / low-level primitive. |
| `write_rename.rs` | ntfs-basic | 8 | Same-length rename in subdir AND root (via `$INDEX_ALLOCATION`), length mismatch rejection, slash rejection, missing source, preservation of other entries. |
| `write_unlink.rs` | ntfs-basic + ntfs-large-file | 7 | Subdir index removal, root `$INDEX_ALLOCATION` removal, MFT record freed, non-resident clusters freed, directory-target refusal, missing-source rejection, post-unlink mount. |
| `write_create.rs` | ntfs-basic | 7 | Subdir creation, preservation of siblings, duplicate rejection, invalid-basename rejection, non-directory-parent rejection, root-refused (MVP), upstream round-trip. |
| `write_mkdir.rs` | ntfs-basic | 5 | mkdir basic, empty-and-listable, duplicate rejection, create-file-inside-new-dir, post-mkdir mount. |
| `write_rmdir.rs` | ntfs-basic | 5 | Remove empty dir, refuse non-empty, refuse regular file, post-rmdir mount, 5x churn doesn't leak MFT records. |
| `write_resident_contents.rs` | ntfs-basic + ntfs-large-file | 5 | Create-then-write, replace existing, expand up to capacity, refuse non-resident, post-write mount. |
| `write_promote.rs` | ntfs-basic + ntfs-large-file | 6 | Small-payload promotion, 10 KiB promotion, refuse already-non-resident, dispatcher resident case, dispatcher auto-promote 2 KiB, post-promote mount. |
| `write_rename_varlen.rs` | ntfs-basic | 7 | Rename to shorter/longer name, delegation-to-same-length, existing-target rejection, invalid-basename rejection, same-name no-op, remount. |
| `write_ads.rs` | ntfs-basic | 7 | Create ADS, multiple ADS with unnamed preserved, overwrite replaces, delete removes, delete-missing errors, empty-name rejected, churn + remount. |
| `write_reparse.rs` | ntfs-basic | 5 | Write reparse point sets flag+attr, remove clears both, remove-missing errors, create_symlink sets state, churn + remount. |

No unit tests inside the crate; all coverage is integration-level.
Coverage gap: see the last paragraph in
[Test infrastructure](#test-infrastructure) — tests drive the upstream
`ntfs` crate, not the `fs_ntfs_*` C ABI.

## Implemented write phases

The W-plan tracked the read-only-to-read-write rollout in five
phases (W0 → W4). W0–W4 are shipped; W2.6 (MFT self-growth) and
W3.2/3.3 (B+ tree insert / delete on overflowed `$INDEX_ALLOCATION`)
are the two outstanding pieces, tracked in
[`FUTURE_FEATURES.md`](FUTURE_FEATURES.md). W5 (`$LogFile`
journaling) was intentionally skipped — `fs_ntfs_fsck` is the
recovery path instead.

### Phase W0 — volume recovery (✅ shipped)

Three C-ABI functions in `src/fsck.rs`:

| Symbol | Behaviour |
|---|---|
| `fs_ntfs_clear_dirty(path) -> c_int` | Clears `VOLUME_IS_DIRTY` in `$Volume/$VOLUME_INFORMATION`. Returns 1 (cleared), 0 (already clean), -1 (error). |
| `fs_ntfs_reset_logfile(path) -> i64` | Overwrites `$LogFile` with `0xFF` — the NTFS format-level "no pending transactions" signal. Returns bytes written. |
| `fs_ntfs_fsck(path, *logfile_bytes, *dirty_cleared) -> c_int` | Runs both; `NULL` out-params are accepted. |

Tests: `tests/fsck.rs` (7) + `tests/capi_fsck.rs` (8).

Notable finding during implementation: upstream's
`NtfsResidentAttributeValue::data_position()` returns the *attribute
header* start, not the resident *data* start — we read `value_offset`
(u16 at header +0x14) ourselves to locate the actual data.

### Phase W1 — in-place writes (✅ shipped)

"Easy" writes: bytes that already exist on disk at known sizes. No
attribute resize, no cluster allocation, no index mutation. Introduces
the MFT-record USA-fixup RMW machinery that every W1+ phase builds on.

#### W1.1 — MFT-record RMW primitive

`src/mft_io.rs`:

- `read_boot_params(path)` — pulls `bytes_per_sector`, `cluster_size`,
  MFT LCN, `file_record_size` from the boot sector (offsets per Windows Internals 7th ed.).
- `read_mft_record(path, n)` — reads record `n`, applies USA fixup
  (validates FILE magic + USN across every 512-byte sector), returns
  clean bytes.
- `update_mft_record(path, n, |rec| Ok(()))` — the core primitive.
  Caller's mutator receives post-fixup bytes, USN is bumped (skipping
  `0` because some drivers treat it as uninitialized), re-applied to
  sector-ends, record is `fsync`'d.
- `apply_fixup_on_read` / `apply_fixup_on_write` exposed for in-memory
  synthesis in tests.

**Safety gates:**

- Refuses to write to records whose `IN_USE` flag (0x0001 at +0x16) is
  clear — prevents accidental corruption of free MFT entries.
- Read fixup rejects torn writes (any sector-end pair not matching USN).
- Read fixup rejects non-`FILE` records.

9 tests: boot-param extraction, `$Volume` record sanity, identity
round-trip byte-preservation (modulo USN bump), upstream-parses-after-
RMW, in-memory synthesis round-trip, torn-write detection, magic check,
free-record refusal, mutator error propagation.

#### W1.2 — `set_times`

`src/write.rs::set_times(path, file_path, FileTimes)` and
`set_times_by_record_number(...)`.

Patches the four u64 NT-time fields (creation / modification /
mft_record_modification / access, at offsets 0/8/16/24 of
`$STANDARD_INFORMATION`'s resident value). Only modifies SI; the
duplicate `$FILE_NAME` times in the parent dir's index are left stale
— matches Windows semantics (Windows only updates the FN copy on
rename/create).

6 tests: all-four round-trip, selective single-field writes preserve
others, remount round-trip, nested path, missing-path rejection,
upstream mounts the post-write image.

#### W1.3 — `set_file_attributes`

`src/write.rs::set_file_attributes(path, file_path, FileAttributesChange)`
and `set_file_attributes_by_record_number(...)`.

Flips bits in `$STANDARD_INFORMATION.file_attributes` (u32 at +0x20).
`add` / `remove` overlap is rejected. Bit values match Windows
`FILE_ATTRIBUTE_*` per MS-FSCC 2.6.

6 tests: add-bit, remove-bit, multiple-in-one-call, overlap-rejection,
remount survival, isolation from unrelated files.

#### W1.4 — `write_at` for existing non-resident data

`src/write.rs::write_at{,_io,_by_record_number,_by_record_number_io}`.
Walks the non-resident `$DATA` mapping-pairs, locates the disk byte
address for `offset`, and writes through `BlockIo`. Refuses
extension past `value_length`, compressed streams, and sparse holes.
Tests in `tests/write_content.rs`.

#### W1 C-ABI surface

`fs_ntfs_set_times`, `fs_ntfs_set_file_attributes`, `fs_ntfs_write_file`
all live in `src/lib.rs`. Driven by `tests/capi_write_w1.rs`,
`tests/capi_write_content.rs`, and friends.

### Phase W2 — attribute mutation (W2.6 outstanding)

**Shipped:**

- **W2.1 resident attribute resize.** `src/attr_resize.rs`:
  `resize_resident_value` / `set_resident_value`. Shifts subsequent
  attributes inside the MFT record, zero-fills the released range,
  updates `bytes_used`. Rejects grows past `bytes_allocated`. 8 tests
  using volume-label rename as the vehicle (same-length / shrink /
  grow / zero-length / round-trip / huge-grow rejection /
  preserves-subsequent-attributes / low-level in-memory primitive).
- **W2.3 `$Bitmap` cluster allocator.** `src/bitmap.rs`:
  `locate_bitmap`, `find_free_run`, `allocate`, `free`, `is_allocated`.
  10 tests.
- **W2.4 data-run encoder.** `data_runs::encode_runs` (inverse of
  `decode_runs`). Round-trip tested for sparse, negative deltas, large
  LCNs, multi-run lists. 9 additional tests.
- **W2.5 truncate (shrink).** `write::truncate{,_by_record_number}`
  and `fs_ntfs_truncate`. Trims runs, frees clusters in `$Bitmap`,
  updates `allocated_length` / `data_length` / `initialized_length` /
  `last_vcn`. MFT-first, bitmap-second ordering. 11 tests (7 Rust-
  layer + 4 C-ABI).
- **W2.5 grow (non-resident).** `write::grow_nonresident{,_by_record_number}`
  and `fs_ntfs_grow`. Allocates a single contiguous free run, extends
  the last data run or appends a new one, updates lengths. Undoes the
  bitmap allocation if the new mapping-pairs don't fit in the attr
  header. New bytes read as zero via `initialized_length`. 5 tests.

**W2 C-ABI surface (✅ shipped).** `fs_ntfs_truncate` (shrink + grow),
`fs_ntfs_grow`, `fs_ntfs_write_file` all live in `src/lib.rs`. Cluster
allocator and attribute-resize remain internal as planned.

W2.2 (resident → non-resident promotion) is also shipped, surfaced
via `write::promote_resident_data_to_nonresident` and
`promote_attribute_to_nonresident` and used by the
`write_file_contents` dispatcher.

### Phase W3 — create / delete / mkdir / rmdir / rename (W3.2/3.3 outstanding)

**Shipped:**

- `rename_same_length` — same-UTF-16-length rename. Patches the parent's
  index entry AND each `$FILE_NAME` attribute on the file's record.
  Dispatches between resident `$INDEX_ROOT` and `$INDEX_ALLOCATION`
  INDX-block scans (incl. root-dir case which NTFS formatter always overflows).
- `unlink` — removes a file: parent index entry removal + truncate-to-0
  + IN_USE clear + `$MFT:$Bitmap` free. Refuses directories.
- `create_file` — new MFT record built from scratch (FILE header +
  USA + `$SI` + `$FILE_NAME` + empty `$DATA` + end marker), inserted
  into parent's resident `$INDEX_ROOT`. Auto-rolls-back on any failure
  step. Refuses parents whose index has overflowed (waits for B+-tree
  insert).
- `mkdir` — parallel of create_file but emits a directory record with
  an empty `$INDEX_ROOT:$I30` (single LAST sentinel entry) and no
  `$DATA`.
- `rmdir` — deletes an empty directory. Verifies emptiness via the
  `$INDEX_ROOT`'s first-entry LAST bit; refuses non-empty and
  overflowed dirs.
- `write_resident_contents` — replaces a file's resident `$DATA`.
- `promote_resident_data_to_nonresident` — allocates clusters, builds
  a non-resident `$DATA` attribute, swaps it into the MFT record.
- `write_file_contents` — high-level dispatcher: tries resident, falls
  back to promotion on capacity error.
- `idx_block` module — INDX block read/update with USA fixup; underpins
  the `$INDEX_ALLOCATION` walks.
- `mft_bitmap` module — `$MFT:$Bitmap` allocator (resident or
  non-resident).
- `record_build` module — synthesizers for fresh FILE records
  (regular + directory) and non-resident `$DATA` attribute blobs.
- `attr_resize::replace_attribute` — swap an attribute's entire body
  (used for resident→non-resident promotion).

C-ABI additions: `fs_ntfs_rename_same_length`, `fs_ntfs_unlink`,
`fs_ntfs_create_file`, `fs_ntfs_mkdir`, `fs_ntfs_rmdir`,
`fs_ntfs_write_resident_contents`, `fs_ntfs_write_file_contents`.

### Phase W4 — ADS / reparse points / xattrs (✅ shipped)

#### W4.1 — Named `$DATA` streams

- `write::write_named_stream_resident(image, path, stream_name, data)` —
  upsert a resident named `$DATA`. New streams get a fresh attribute
  id; existing streams are replaced via `attr_resize::set_resident_value`.
- `write::delete_named_stream(image, path, stream_name)` — remove a
  named stream attribute from the record.
- Helpers: `attr_resize::insert_attribute_before_end` (locates
  `0xFFFFFFFF` by walking the chain, shifts it forward, inserts a new
  attribute) and `allocate_attribute_id` (bumps the record's
  `next_attr_id`).

C-ABI: `fs_ntfs_write_named_stream`, `fs_ntfs_delete_named_stream`.

MVP limitation: the resident-only path was the original W4.1 MVP;
non-resident named streams are now handled by `write_named_stream`
via the standard promotion machinery.

#### W4.2 — Reparse points + symlinks

- `write::write_reparse_point(image, path, tag, data)` — upsert a
  resident `$REPARSE_POINT` and set `FILE_ATTRIBUTE_REPARSE_POINT`.
- `write::remove_reparse_point(image, path)` — reverse.
- `write::create_symlink(image, parent, basename, target, relative)` —
  create_file + write_reparse_point with `IO_REPARSE_TAG_SYMLINK`
  data. Rolls back the file create on failure.
- Helpers: `record_build::build_resident_reparse_point_attribute`,
  `build_symlink_reparse_data` (SymbolicLinkReparseBuffer per
  MS-FSCC 2.1.2.4), `reparse_tag` module with common tag constants
  (SYMLINK / MOUNT_POINT / WOF / LX_SYMLINK / APPEXECLINK).

C-ABI: `fs_ntfs_write_reparse_point`, `fs_ntfs_remove_reparse_point`,
`fs_ntfs_create_symlink`.

#### W4.3 — Extended attributes

`src/ea_io.rs` (encode/decode/upsert/remove + `$EA_INFORMATION`
synthesis) plus `src/write.rs::write_ea` / `remove_ea` / `list_eas`.
C-ABI: `fs_ntfs_write_ea`, `fs_ntfs_remove_ea`. Tests in
`tests/write_ea.rs` and `tests/capi_ea.rs`.

#### Bonus shipped (not originally in the W-plan)

- **Hardlinks.** `write::link` + `fs_ntfs_link`: adds an extra
  `$FILE_NAME` to the target record and inserts the entry into the
  new parent's index, bumping the hard-link count.
- **Object ID read.** `write::read_object_id` + `fs_ntfs_read_object_id`
  pulls the 16-byte `$OBJECT_ID` GUID when present.
- **Mkfs.** `fs_ntfs_mkfs` formats a fresh volume through the
  block-device callback adapter (see `src/mkfs.rs`).
- **Upcase-table collation (full Unicode).** `src/upcase.rs` ships a
  canonical 64 KiB `$UpCase` table plus a runtime loader that pulls
  the volume's actual `$UpCase` from MFT record 10 when it differs.
  `compare_names` folds each UTF-16 code unit through the table and
  compares; both `insert_entry_into_index_root_with_collation` and
  the INDX-block insert path consume it. Closes the silent
  data-loss path on non-ASCII filenames (`café`, CJK, emoji) where
  insertion sort previously diverged from Windows' binary search.
  Tests in `tests/upcase.rs`.
- **Volume-scale corruption tests.** `tests/corruption_fuzz.rs`
  exercises bit-flipped images and asserts no panics. Backed by
  `_corrupt_*.img` fixtures in `test-disks/`.

---

## Suggested upgrade order (for a future agent)

Revised after the test-infrastructure work and
[Documentation cross-check](#documentation-cross-check) landed. The
cross-check surfaced correctness bugs that outrank any new-feature
work — fix those first.

<!--
COMMENTED OUT 2026-05-02: this entry has been promoted into the new
"## Implemented write phases" section above (which also covers
W1/W2/W3/W4). Kept here as a historical pointer so anyone looking
for "Phase W0" inside "Suggested upgrade order" — its original home
— still finds the trail.

### Phase W0 — volume recovery (✅ shipped)

First step outside the read-only envelope. Narrow raw-byte writes for
volumes that lost their mount handle without a clean unmount.

- `fs_ntfs_clear_dirty(path)` — clear `VOLUME_IS_DIRTY` (0x0001) in
  `$Volume/$VOLUME_INFORMATION`. Returns `1` (cleared), `0` (already
  clean), `-1` (error).
- `fs_ntfs_reset_logfile(path)` — overwrite `$LogFile` with `0xFF`, the
  format-level "no pending transactions" pattern documented in Windows Internals 7th ed.
  Returns bytes written or `-1`.
- `fs_ntfs_fsck(path, *logfile_bytes, *dirty_cleared)` — both above,
  with optional out-params. `NULL` out-params accepted.

Scope is the weakest possible recovery: no `$LogFile` replay, no
MFT/MFTMirror reconciliation. Uncommitted metadata changes are
discarded — the volume becomes mountable again, but some state may
be lost. This matches the recovery posture of comparable recovery tools (and is
what Windows/NTFS driver are designed to accept).

**Tests:** `tests/fsck.rs` (7 Rust-layer) + `tests/capi_fsck.rs` (8
C-ABI) — each exercises a different failure mode (dirty flag set,
corrupted log-first-page, combined). Dirty state is synthesized in
the test via upstream-only APIs, independent of the code under test.
-->

### Phase 1 — correctness fixes (block new features until done)

Each item maps to a finding above; the fixture to regression-test
against is named in brackets.

1. **Widen timestamps to 64-bit seconds + nanoseconds.** Covers the
   Y2038 / wraparound / 100 ns-precision loss bugs in one ABI break.
   `[basic]` + a new fixture with a pre-1970 `touch -d` file.
<!-- 2. **Fix junction / WOF / reparse-point type dispatch.** Read
   `$REPARSE_POINT.tag`; only `IO_REPARSE_TAG_SYMLINK` maps to symlink.
   Ship alongside the `fs_ntfs_readlink` implementation so both land
   together. Needs a new fixture (junction via `ln -T` on an
   NTFS driver-mounted volume).
COMMENTED OUT: shipped. src/lib.rs:484-499 reads the 4-byte tag
from $REPARSE_POINT and dispatches: SYMLINK (0xA000000C) →
FS_NTFS_FT_SYMLINK, MOUNT_POINT (0xA0000003) → FS_NTFS_FT_JUNCTION
(new enum value at include/fs_ntfs.h:31), other tags fall through
to the underlying file/dir type. fs_ntfs_readlink also fully
decodes both SymbolicLinkReparseBuffer and MountPointReparseBuffer
(src/lib.rs:985+). tests/readlink.rs covers it. -->
<!-- 3. **Replace seek-by-reading with `NtfsAttributeValue::seek`** in
   `fs_ntfs_read_file`. `[large-file]` — add a test that reads at a
   multi-MiB offset and times out on quadratic behavior.
COMMENTED OUT: shipped. src/lib.rs:956 calls
`data_value.seek(&mut bridge.reader, SeekFrom::Start(offset))` via
NtfsReadSeek. The old skip-by-read loop is gone. -->
<!-- 4. **Normalize path-traversal handling.** Skip `.`; resolve `..` via
   `parent_directory_reference`. Add a capi test that `/a/./b` and
   `/a/../b` resolve correctly.
COMMENTED OUT: shipped. tests/path_dots.rs exercises `.` / `..`
component handling in path resolution. -->
5. **Handle ADS-only files and widen the dirent name buffer** —
   conceptually one PR: widen `fs_ntfs_dirent_t::name` to 1024 bytes,
   and make `fs_ntfs_read_file` fall back to WOF's `WofCompressedData`
   (or at minimum not error with a zero-length result). `[ads]` +
   `[unicode]` regressions, plus a new 90-character CJK fixture.
   <!-- Status: still outstanding. include/fs_ntfs.h:53 still has
   `name[256]`; no WOF fallback in src/lib.rs read path. -->

**Phase 1 status:** items 2/3/4 shipped; items 1 and 5 still
outstanding.

### Phase 2 — coverage + reliability scaffolding

<!-- 6. **`capi_*` tests.** Parallel suite that drives the shipped
   `fs_ntfs_*` C ABI (not upstream `ntfs` directly). Closes the
   reliability gap flagged under [Test infrastructure](#test-infrastructure).
   Same fixtures, tests link the `rlib` half of the crate and call the
   `extern "C" fn`s with `CString` paths / raw-pointer out-params.
COMMENTED OUT: shipped. tests/ contains capi_ads, capi_create,
capi_ea, capi_fsck, capi_fsck_callbacks, capi_link, capi_remove,
capi_rename, capi_reparse, capi_write_content, capi_write_truncate,
capi_write_variants, capi_write_w1 — all driving the `fs_ntfs_*`
C ABI directly. -->
7. **Re-enable `clippy -D warnings` in CI.** Resolve the
   `not_unsafe_ptr_arg_deref` lints (mark FFI entries `unsafe extern "C"`
   or allow the lint at the boundary). Without this gate, regressions
   go uncaught.
   <!-- Status: still outstanding. The crate-level
   `#![allow(clippy::not_unsafe_ptr_arg_deref)]` exists at
   src/lib.rs:12, but `.github/workflows/ci.yml` still has the
   `cargo clippy` step commented out ("clippy deferred… Re-enable
   once resolved"). Flip the CI step on. -->
8. **Dirty-volume state surface.** Add `fs_ntfs_volume_info_t::is_dirty`
   + optional `metadata_consistent` from an MFT-mirror cross-check.
   Requires a fresh fixture with `$Volume` dirty bit set (either force
   via NTFS formatter options, or a file attribute tools-style flag via NTFS driver).
   <!-- Partial: standalone `fs_ntfs_is_dirty(path)` and
   `fs_ntfs_is_dirty_with_callbacks(cfg)` shipped (header lines
   193, 289). The struct field
   `fs_ntfs_volume_info_t::is_dirty` and the mirror cross-check
   `metadata_consistent` are still missing. -->

**Phase 2 status:** item 6 shipped; items 7 (CI flip) and 8
(struct-field surface + mirror cross-check) still outstanding.

### Phase 3 — new features

9. **Lazy directory iteration.** Stop materializing entire indexes in
   `dir_open`; retain the iterator. `[manyfiles]` regression + a new
   ≥100k-entry fixture (32 MiB image easily holds it).
   <!-- Status: still outstanding. src/lib.rs:1175-1265 still materialises
   every entry into a `Vec<FsNtfsDirent>` (struct at lib.rs:415) inside
   `fs_ntfs_dir_open` before returning the iterator. C-ABI shape change
   needed: store the upstream `NtfsIndexEntries` iterator inside
   `FsNtfsDirIter` and advance in `fs_ntfs_dir_next` (lib.rs:1286). -->
10. **ADS read API.** `fs_ntfs_read_stream(fs, path, stream_name, …)` +
    `fs_ntfs_list_streams(…)`. The `ntfs-ads.img` fixture + Rust-layer
    tests already exist; add the C surface + a `capi_ads.rs`.
    <!-- Partial: capi_ads.rs exists, and the WRITE side is shipped
    (fs_ntfs_write_named_stream / fs_ntfs_delete_named_stream at
    include/fs_ntfs.h:407, 414). The READ-side
    fs_ntfs_read_stream / fs_ntfs_list_streams C surface is still
    missing. -->
<!-- 11. **Structured errno companion.** `fs_ntfs_last_errno()` returning
    POSIX errno. Mirrors `fs_ext4_last_errno()`.
COMMENTED OUT: shipped. include/fs_ntfs.h:177 declares
`fs_ntfs_last_errno()`; src/lib.rs:132 implements it; also
`fs_ntfs_clear_last_error()` at h:182 / lib.rs:139.
tests/errno_companion.rs covers it. -->
12. **POSIX mode from `FILE_ATTRIBUTE_READONLY` + extensions.** Minimal
    derivation first (drop write bits, set exec for `.exe`/`.bat`/etc);
    full `$SECURITY_DESCRIPTOR` parsing later.
    <!-- Status: still outstanding. -->
<!-- 13. **Synthesize `.` / `..` in `dir_open`.** One-liner once parent-ref
    lookup exists.
COMMENTED OUT: shipped. src/lib.rs:807-828 synthesises both
entries at the head of every `fs_ntfs_dir_open` listing,
resolving `..` via `parent_record_number_of`.
tests/readdir_dots.rs covers it. -->
14. **WOF decompression.** Parse `IO_REPARSE_TAG_WOF` data; decompress
    `WofCompressedData` via `ms-compress` or similar. Needed for
    correctness on modern Win10/11 volumes. Probably biggest single
    engineering task in the list.
    <!-- Status: still outstanding. src/lib.rs:492 acknowledges WOF
    explicitly ("WOF / LX_SYMLINK / APPEXECLINK / dedup etc. — leave
    file_type at whatever it was") but no decompression path. -->

**Phase 3 status:** items 11 and 13 shipped; 9, 10 (read-side),
12, 14 still outstanding.

### Phase 4 — polish

<!-- 15. ~~**Wire up or delete `best_file_name_str`.**~~ Deleted.
COMMENTED OUT: already crossed out as deleted; nothing further to
track. -->
16. **Universal release builds on tag push.** Release workflow was
    cloned from fs-ext4; verify it produces
    `libfs_ntfs-v0.1.0-macos-universal.tar.gz` on a tag.
    <!-- Status: unverified — `.github/workflows/` only contains
    ci.yml; if a release.yml ever existed it isn't here now. Either
    a tag-push job was never wired up, or it lives in a different
    repo. Confirm before assuming this is shipped. -->
<!-- 17. **Rust-native facade.** Thin idiomatic wrapper around the C ABI
    (`Filesystem::mount(path) -> Result<Filesystem>`, `File::read`).
COMMENTED OUT: shipped. tests/facade.rs exercises the
Rust-native facade. -->
<!-- 18. **Writes.** Only attempt after upstream `ntfs` supports writes,
    or after forking. Not a near-term goal.
COMMENTED OUT: emphatically shipped — the entire write surface
(W0/W1/W2/W3) was added in this crate without waiting for upstream.
include/fs_ntfs.h now exports mkfs, set_times, set_file_attributes,
create_file, mkdir, rmdir, unlink, rename / rename_same_length,
truncate, grow, write_file, write_file_contents,
write_resident_contents, write_named_stream / delete_named_stream,
write_reparse_point / remove_reparse_point, create_symlink,
write_ea / remove_ea, link, read_object_id, plus handle-based `_h`
siblings. See the "Write path" tests table above. -->

**Phase 4 status:** items 15, 17, 18 shipped; item 16 (universal
release builds) needs verification — no release workflow file
present.

## Documentation cross-check

Cross-check against
[MS-FSCC](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-fscc/),
Windows Internals 7th ed. ch. "NTFS On-Disk Structure",
and the [upstream `ntfs` 0.4 API](https://docs.rs/ntfs/0.4/ntfs/).
Findings below are in addition to the items already listed under
[Known limitations](#known-limitations).

### Correctness bugs

#### Timestamps truncated to 32-bit UNIX epoch
**Location:** `src/lib.rs:431-437` (`filetime_to_unix`), `include/fs_ntfs.h:37-40`
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
(MS-FSCC §2.4.4 / Windows Internals 7th ed.).
Worst-case UTF-8 encoding is 255 × 4 = **1020 bytes** plus NUL.
**Code:** `name[256]`; copy capped at 255 bytes with no error signal.
**Why it matters:** A file with non-BMP names (emoji, rare CJK) or long
CJK names (as few as 86 characters exceeds 255 UTF-8 bytes) comes back
with a corrupted, non-roundtrippable name. A subsequent `fs_ntfs_stat`
on that truncated name fails with ENOENT. This is a **silent data-loss
path** hit by any FSKit enumeration of user profile / OneDrive dirs.
**Fix:** Widen to `name[1024]` (other NTFS implementations uses this), or return
variable-length with a caller-owned buffer.

<!-- #### Junctions / mount points / WOF files mis-reported as symlinks
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
COMMENTED OUT: fixed. src/lib.rs:484-499 reads the 4-byte tag and
dispatches; FS_NTFS_FT_JUNCTION (=8) added at include/fs_ntfs.h:31;
WOF / LX_SYMLINK / AppExecLink / dedup tags fall through to the
underlying file/dir type. WOF *decompression* is still outstanding
(see Phase 3 #14 and the "WOF" entry under correctness risks
below) but the type-dispatch bug itself is closed. -->

<!-- #### Path traversal: `..` and `.` looked up literally
**Location:** `src/lib.rs:212-228`
**Spec:** NTFS `$INDEX_ALLOCATION` does **not** store `.` / `..`
entries (per Windows Internals 7th ed. ch. "NTFS On-Disk Structure");
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
COMMENTED OUT: fixed. tests/path_dots.rs locks in
`.` / `..` handling in path resolution. -->

<!-- #### Seek-by-reading in `fs_ntfs_read_file`
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
COMMENTED OUT: fixed. src/lib.rs:956 now does
`data_value.seek(&mut bridge.reader, SeekFrom::Start(offset))`. -->



### Correctness risks

#### `NtfsFileNameIndex::find` matches DOS 8.3 names
**Location:** `src/lib.rs:221-224`
**Spec:** A file with a DOS shortname has two `$FILE_NAME` entries
— one per namespace
(per Windows Internals 7th ed. ch. "NTFS On-Disk Structure").
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
(per Windows Internals 7th ed.).
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
**Location:** `src/lib.rs:524` (`fill_attr` reparse-tag dispatch) + `src/lib.rs:1315` (`fs_ntfs_read_file`)
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
**Location:** `src/lib.rs:1175-1265` (`fs_ntfs_dir_open`); iterator struct at `src/lib.rs:415`
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
(per Windows Internals 7th ed.).
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
<!-- PARTIAL: dirty-state probe is now reachable via standalone
`fs_ntfs_is_dirty(path)` (header:193) and
`fs_ntfs_is_dirty_with_callbacks(cfg)` (header:289). What still
matches this finding: the bit is NOT exposed as a field on
`fs_ntfs_volume_info_t` (so a normal mount + get_volume_info
caller can't see it), and there is no MFT/MFTMirr cross-check or
`metadata_consistent` companion. Leave the entry; only the
"no dirty check at all" line is obsolete. -->


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
(per Windows Internals 7th ed. ch. "NTFS Security");
NTFS driver derives POSIX mode by mapping the DACL against a `UserMapping`
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

<!-- #### `fs_ntfs_readlink` stub — no reparse-point readback at all
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
COMMENTED OUT: shipped. src/lib.rs:985+ implements full
`fs_ntfs_readlink`: walks attributes, finds `$REPARSE_POINT`,
decodes tag, dispatches to decode_symlink_print_name /
decode_mount_point_print_name. tests/readlink.rs covers it. -->


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

<!-- #### `.` and `..` not synthesized in directory listings
**Location:** `src/lib.rs:545-582`
**Spec:** POSIX `readdir(3)` expects `.` and `..`; NTFS indexes don't
store them.
**Code:** Returns neither.
**Why it matters:** POSIX consumers (FSKit's BSD-derived enumeration,
`fts(3)` ports) that rely on entry count / presence break silently.
**Fix:** Prepend synthesized entries in `dir_open`:
`{name=".", frn=dir.frn}`, `{name="..", frn=parent.frn}` where parent
comes from the dir's `$FILE_NAME.parent_directory_reference`.
COMMENTED OUT: shipped. src/lib.rs:807-828 prepends both entries
in every `fs_ntfs_dir_open` listing; parent FRN comes from
`parent_record_number_of` (which reads
`$FILE_NAME.parent_directory_reference`). tests/readdir_dots.rs
covers it. -->


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
