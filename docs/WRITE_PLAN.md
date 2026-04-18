# fs-ntfs — write-support plan (W0 → W4)

Phased implementation plan for turning fs-ntfs from a read-only crate
into a write-capable NTFS driver. Track for this plan is updated as
phases land. Every sub-task has its own commit in git so progress is
granular and rollback-safe.

**Guiding constraints** (stated by the project owner):

- **No GPL code consulted.** Use MS-FSCC, Flatcap, and upstream `ntfs`
  (MIT/Apache-2.0) only. `ntfs-3g` (GPL) is off-limits. If an
  implementation gets stuck without consulting GPL, pause and write a
  scoped question to `docs/wip-notes/` (gitignored) for discussion
  instead of committing a GPL-tainted shortcut.
- **Tests against real disk images.** Fixtures live in `test-disks/`
  and are produced by `test-disks/build-ntfs-feature-images.sh` (a
  qemu+Alpine VM running `mkntfs` + `ntfs-3g` mount-for-populate). The
  same script runs in CI and on any dev machine with `qemu`.
- **Dev-loop every step.** A baseline contract in `/tmp/tests_baseline.txt`
  captures every currently-passing test name; it's verified after every
  change; it only grows. See `/Users/.../skills/dev-loop` for the exact
  procedure used. 48 tests → 60 tests → … onward.
- **Skip W5 (journaling).** Crash-safety via `$LogFile` writeback +
  replay is months of work and requires careful consultation of
  specs; we document writes as best-effort and rely on `fs_ntfs_fsck`
  for post-crash repair instead.

---

## Phase W0 — volume recovery (✅ shipped)

Already in `main`. Three C-ABI functions in `src/fsck.rs`:

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

---

## Phase W1 — in-place writes (in progress)

"Easy" writes: bytes that already exist on disk at known sizes. No
attribute resize, no cluster allocation, no index mutation. Introduces
the MFT-record USA-fixup RMW machinery that every W1+ phase builds on.

### W1.1 — MFT-record RMW primitive (✅)

`src/mft_io.rs`:

- `read_boot_params(path)` — pulls `bytes_per_sector`, `cluster_size`,
  MFT LCN, `file_record_size` from the boot sector (offsets per Flatcap).
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

### W1.2 — `set_times` (✅)

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

### W1.3 — `set_file_attributes` / chattr (✅)

`src/write.rs::set_file_attributes(path, file_path, FileAttributesChange)`
and `set_file_attributes_by_record_number(...)`.

Flips bits in `$STANDARD_INFORMATION.file_attributes` (u32 at +0x20).
`add` / `remove` overlap is rejected. Bit values match Windows
`FILE_ATTRIBUTE_*` per MS-FSCC 2.6.

6 tests: add-bit, remove-bit, multiple-in-one-call, overlap-rejection,
remount survival, isolation from unrelated files.

### W1.4 — `write_at` for existing non-resident data (pending)

Content rewrite at a given byte offset within the existing logical
size of a file's unnamed `$DATA`. Non-resident only (resident files
stay in W2 — extending them touches attribute-resize machinery).

Plan:

1. Walk `$DATA`'s non-resident attribute in the MFT record via
   `attr_io`.
2. Parse the mapping-pairs list from `non_resident_mapping_pairs_offset`
   forward (standard NTFS data-run encoding: first nibble-byte gives
   byte-counts for length and LCN delta; decode into `Vec<(Lcn, Vcn)>`).
3. For the given offset, find which run it lands in; compute disk byte
   address.
4. Open RW, seek, write, fsync. No MFT touching required — data runs
   point at clusters outside the MFT.

**Refuses:**
- offset + len > current `value_length` (no extension)
- compressed (`compression_unit != 0` in header) — requires
  decompress/recompress pipeline
- sparse holes in the target range — writing bytes into a hole would
  implicitly allocate clusters (W2 territory)

**Tests (planned):**
- Rewrite middle of `ntfs-large-file.img`'s 8 MiB `big.bin`, verify
  upstream reads back the same bytes.
- Rewrite across a data-run boundary.
- Reject writes past EOF.

### W1 C-ABI surface (pending)

Single commit at end of W1:

```c
int fs_ntfs_set_times(const char *image, const char *path,
                      const int64_t *creation,
                      const int64_t *modification,
                      const int64_t *mft_record_modification,
                      const int64_t *access);

int fs_ntfs_chattr(const char *image, const char *path,
                   uint32_t add_flags, uint32_t remove_flags);

int64_t fs_ntfs_write_file(const char *image, const char *path,
                           uint64_t offset, const void *buf, uint64_t len);
```

Plus `capi_write_times.rs`, `capi_write_attrs.rs`, `capi_write_content.rs`
driving them.

---

## Phase W2 — attribute mutation (in progress)

**Shipped so far:**

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

**W2.1 resident attribute resize (shipped).** `src/attr_resize.rs`:
`resize_resident_value` / `set_resident_value`. Shifts subsequent
attributes inside the MFT record, zero-fills the released range,
updates `bytes_used`. Rejects grows past `bytes_allocated`. 8 tests
using volume-label rename as the vehicle (same-length / shrink /
grow / zero-length / round-trip / huge-grow rejection / preserves-
subsequent-attributes / low-level in-memory primitive).

**Remaining in W2:**

### W2.1 — Resident grow / replace in-record

If the new data fits in the MFT record's free space (`bytes_allocated
- bytes_used`), shift any following attributes, rewrite the header's
`value_length` + attribute `length`, update `bytes_used`, preserve the
trailing `0xFFFFFFFF` end marker. Existing resident attribute order
can't change (attribute_id stays the same).

### W2.2 — Resident → non-resident promotion

When data no longer fits inline, allocate clusters (W2.3), copy data
there, rewrite the attribute header in place as non-resident (different
layout), encode a single-run mapping-pairs list, reclaim the previously
resident bytes within the record.

### W2.3 — Cluster allocator against `$Bitmap`

`$Bitmap` (record #6) holds 1 bit per cluster (0 = free). Need:

- `find_free_run(n_clusters, hint_lcn)` — scan for n contiguous free
  bits. Best-fit or first-fit with locality hint. For W2 the simple
  first-fit works; fragmentation avoidance is W4 polish.
- `allocate(range)` / `free(range)` — flip bits, rewrite the affected
  4 KiB sectors of `$Bitmap`'s non-resident data runs, `fsync`.

All bitmap mutations must themselves honor the data-run mapping (the
bitmap is itself non-resident) — so W1.4's data-run walker is a hard
prerequisite for W2.3.

### W2.4 — Data-run encoding (inverse of W1.4 decoder)

Given a `Vec<(Lcn, NumClusters)>`, emit NTFS mapping-pairs bytes. LCN
is stored as a signed delta from the previous run's LCN — the first
run's LCN is stored absolute. Compact encoding: nibble-header byte
specifies byte counts for length and LCN fields (1–8 each), followed
by the bytes in little-endian.

### W2.5 — Non-resident grow / shrink / truncate

- **Grow**: allocate extra clusters via W2.3, append to run list,
  update `value_length`/`allocated_length`/`initialized_length`. Fill
  new clusters with zeros (VCNs past `initialized_length` are read as
  zero, but Windows expects us to have zeroed them on allocation).
- **Shrink**: trim runs from the end, free those clusters in the
  bitmap, rewrite lengths. Partial-run boundary cases are the bug farm.
- **Truncate to zero**: free all runs, set lengths to 0.

### W2.6 — MFT self-growth

When there are no free MFT records, MFT itself must grow. `$MFT`'s own
`$Bitmap` tracks records; when full, allocate a cluster via W2.3,
extend `$MFT`'s `$DATA` runs, add bits to its `$Bitmap`. This is
recursion — careful ordering required to avoid trying to allocate the
bitmap bit for a record that doesn't yet exist.

### W2 C-ABI surface

- `fs_ntfs_truncate(image, path, new_size)` — shrink / grow
- `fs_ntfs_write_file` extended with `O_APPEND`-style grow semantics
- (cluster allocator / attribute-resize are internal — no C surface)

### W2 fixtures

- `ntfs-w2-testbed.img` — volume with known free bitmap regions (some
  files deleted to leave fragmentation). Add to `_vm-builder.sh`.
- `ntfs-small-free.img` — tiny volume with only enough free clusters
  for W2 growth tests to detect allocation failures.

**Expected size:** ~2200 LOC, 2 weeks of a focused human.

---

## Phase W3 — create / delete / mkdir / rmdir / rename (planned)

Built on top of W2 (needs cluster allocator + attribute resize) plus
the single-hardest piece of NTFS write support: `$INDEX_ROOT` /
`$INDEX_ALLOCATION` B+ tree mutation.

### W3.1 — Create regular file

1. Allocate MFT record via W2.6's bitmap flip.
2. Write file record header: FILE magic, USA, `IN_USE` flag, link
   count 1, a fresh `attribute_id` counter.
3. Add `$STANDARD_INFORMATION` (resident, 72 bytes on 3.x+).
4. Add `$FILE_NAME` (resident, 66+2n bytes for an n-char name).
5. Add an empty resident `$DATA`.
6. Insert the file's `$FILE_NAME` as an entry in the parent dir's
   index (W3.2).
7. Bump parent dir's link count in its MFT record.

### W3.2 — `$INDEX_ALLOCATION` B+ tree insert

Two cases:

- **Small dir** (fits in `$INDEX_ROOT`): insert into the resident tree.
  If it no longer fits, promote to `$INDEX_ALLOCATION` (much like
  resident → non-resident).
- **Large dir**: walk B+ tree from root. At each node, binary-search
  by the NTFS collation rule (typically `COLLATION_FILE_NAME`:
  case-insensitive upcase-table comparison). At leaf:
  - If node has free space: insert, rewrite node, done.
  - If not: split. Allocate a new index node (uses `$Bitmap` attribute
    scoped to `$INDEX_ALLOCATION`), pick a median key, propagate the
    new key up. Recurse on parent if it also splits.

Splitting the root promotes it from resident `$INDEX_ROOT` to
non-resident `$INDEX_ALLOCATION` — same machinery as W2.2.

### W3.3 — `$INDEX_ALLOCATION` B+ tree delete

Remove the entry in the leaf, then rebalance on underflow: merge with
sibling or rotate. Merge-and-shrink toward root. If root becomes
resident-sized again, demote back to `$INDEX_ROOT`.

### W3.4 — `fs_ntfs_unlink` / `fs_ntfs_rmdir`

- Remove parent-dir index entry (W3.3).
- Free all file's data-run clusters (W2.5 truncate-to-zero).
- Free the MFT record (`IN_USE` flag clear, flip `$MFT:$Bitmap`).
- For `rmdir`: require the dir's index holds zero real entries.
  (NTFS indexes don't store `.` / `..`, so "empty dir" == "empty index".)

### W3.5 — `fs_ntfs_mkdir`

Same as create-file but:
- `flags |= FILE_NAME_DIRECTORY` in `$FILE_NAME`.
- No `$DATA`; instead an empty `$INDEX_ROOT` for the `$I30` index.
- Link count 2 (`.` is notional; NTFS doesn't really use it the way
  POSIX does but the count still semantically includes self-reference).

### W3.6 — `fs_ntfs_rename`

- Same directory + same-length name: patch the name bytes in both
  the parent's index entry and the file's `$FILE_NAME` attribute.
- Different directory OR different-length name: delete old index
  entry (W3.3), patch `$FILE_NAME.parent_directory_reference`, insert
  into new parent's index (W3.2).

### W3 fixtures

- `ntfs-w3-empty.img` — fresh 32 MiB volume with only root. Tests
  create files into this, verify upstream reads them back.
- `ntfs-w3-deep-tree.img` — pre-built 100k-file dir to stress-test
  B+ tree walks.
- `ntfs-w3-full.img` — volume near capacity, for "allocation failure"
  negative tests.

**Expected size:** ~2200 LOC. 2–3 weeks.

---

## Phase W4 — ADS / reparse points / xattrs (planned)

All layered on W2 machinery:

### W4.1 — Named `$DATA` streams

Creating `file:streamname` is "add a named `$DATA` attribute to an
existing MFT record". Same resize machinery as W2.1/2.2.

### W4.2 — Reparse points

Creating a symlink / junction: add `$REPARSE_POINT` attribute (resident
for short targets, non-resident for long), set
`FILE_ATTRIBUTE_REPARSE_POINT` in SI. Tag + target buffer per
MS-FSCC 2.1.2.4 / 2.1.2.5.

### W4.3 — Extended attributes (`$EA` / `$EA_INFORMATION`)

Add `$EA_INFORMATION` (resident) + `$EA` attributes. Format documented
in Flatcap.

**Expected size:** ~500 LOC. 1 week.

---

## Phase W5 — journaling (skipped)

Would require implementing `$LogFile` restart page synthesis, log
record append, and replay-on-mount. Multiple months; extensive
interoperability risk with Windows. Not blocking any near-term
consumer — `fs_ntfs_fsck` is a sufficient recovery path.

---

## Testing strategy across phases

Each new operation gets **two** test files:

1. `tests/<op>.rs` — Rust-layer tests exercising the function directly.
   Test setup uses **upstream-only APIs** so setup is independent of
   the code under test.
2. `tests/capi_<op>.rs` — same scenarios driven through the `fs_ntfs_*`
   C ABI. Uses `CString`, raw out-pointers, `fs_ntfs_last_error`.

**Round-trip validation.** Every write test reads back via upstream
`ntfs` (independent parser). If both our write AND upstream's read
are correct, the values match. If either is wrong, the test fails.

**Fixture strategy.**
- Base fixtures (`ntfs-basic.img`, `ntfs-large-file.img`, etc.) are
  built once by the qemu+Alpine pipeline.
- Per-test variations are synthesized at runtime by copying a base
  fixture and patching specific bytes via upstream-only code.
- Completely-new fixture requirements (new feature combinations) get
  a new `build_*` in `_vm-builder.sh` + an entry in `ALL`.

**CI.** `.github/workflows/ci.yml` runs the fixture build, then
`cargo test --release`. Alpine VM assets are cached. Fresh run ~60s;
cached ~30s.

---

## Commit granularity & rollback

Each W-sub-task lands as its own commit (or a small cluster of related
commits for setup + implementation + tests). Baseline test count is
announced in commit messages so rollback points are easy to spot in
`git log --oneline`.

If a bug is discovered only after subsequent work lands on top:

1. `git revert <bad commit>` is preferred over reset.
2. The revert commit explicitly references the STATUS.md entry it
   invalidates, so the doc can be updated in the next commit.

---

## Audit trail (chat-only, not in git)

Per owner's request: sources consulted for each phase are logged
**only in the conversation that produced the code**, not in commit
messages or source comments. Source citations in code point at public
docs (Flatcap, MS-FSCC, upstream ntfs) — never at `ntfs-3g`.

---

_Last updated: tracks live state as phases land._
