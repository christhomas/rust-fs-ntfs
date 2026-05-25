# fs-ntfs — future features / outstanding work

What's *not yet* implemented in the write surface and what is needed
to close the gap. Live status of what already ships is in
[`STATUS.md`](STATUS.md) (see "Implemented write phases" + the
"Writes" / "Recovery / volume tools" tables under "What the C ABI
exposes today").

This file used to be `WRITE_PLAN.md` — a forward-looking W0→W4
roadmap. W0–W4 have shipped (modulo the items listed below), so the
plan-style content moved to STATUS.md and this doc was renamed to
make its scope obvious.

**Guiding constraints** (still in force for any new work below):

- **No GPL code consulted.** Use MS-FSCC, Windows Internals 7th ed.,
  and upstream `ntfs` (MIT/Apache-2.0) only. GPL'd NTFS reimplementations
  are off-limits. If an
  implementation gets stuck without consulting GPL, pause and write a
  scoped question to `docs/wip-notes/` (gitignored) for discussion
  instead of committing a GPL-tainted shortcut.
- **Tests against real disk images.** Fixtures live in `test-disks/`
  and are produced by `test-disks/build-ntfs-feature-images.sh` (a
  qemu+Alpine VM running `NTFS formatter` + `NTFS driver`
  mount-for-populate). The same script runs in CI and on any dev
  machine with `qemu`.
- **Dev-loop every step.** A baseline contract in
  `/tmp/tests_baseline.txt` captures every currently-passing test
  name; it's verified after every change; it only grows. See
  `/Users/.../skills/dev-loop` for the exact procedure used.

---

## Outstanding write-surface work

| ID | Item | Why it's blocking |
|---|---|---|
| **W2.6** | MFT self-growth | `create_file` / `mkdir` fail with `"MFT full — would need to grow $MFT (W2.6)"` once `$MFT:$Bitmap` is exhausted (see `src/write.rs:903`, `:1098`). |
| **W3.2** | `$INDEX_ALLOCATION` B+ tree insert | `create_file` / `mkdir` / `rename` refuse to operate on parents whose `$INDEX_ROOT` has already overflowed into `$INDEX_ALLOCATION` (the `IH_FLAG_HAS_SUBNODES` check at `src/write.rs:884`, `:1082`, `:1514`). |
| **W3.3** | `$INDEX_ALLOCATION` B+ tree delete + rebalance | Symmetrical to W3.2; `rmdir` / `unlink` / `rename`-out-of-overflowed-dir all need it. |
| **W3 fixtures** | `ntfs-w3-empty.img`, `ntfs-w3-deep-tree.img`, `ntfs-w3-full.img` | Required to exercise the B+ tree code paths above (especially the 100k-entry stress for splits). |
| **W4 polish** | Native non-resident named-stream synthesis path | `write_named_stream` already promotes when needed via the W2 machinery. Direct synthesis (skip the round-trip) would be cleaner but isn't blocking. |

The two genuinely hard items are W2.6 and W3.2/3.3; once they land
the existing higher-level ops (`create_file`, `mkdir`, `rename`)
lose their fail-fast paths automatically.

---

### W2.5 — long-filename / attribute-list edge case

Resident-only attributes that could outgrow an MFT record:

- `$FILE_NAME` — up to 255 UTF-16 code units (≈ 510 bytes payload +
  66 byte $FILE_NAME header + 24 byte attribute header). With
  multiple hard links + a large `$SECURITY_DESCRIPTOR`, a 1024-byte
  record could in principle exhaust. `$FILE_NAME` is required to
  be resident per MS-FSCC, so the answer when this happens is an
  `$ATTRIBUTE_LIST` (extension record), not promotion.
- `$ATTRIBUTE_LIST` itself can be non-resident if it grows.

**Bounds guard shipped** (2026-05-23): `build_regular_file_record`
and `build_directory_record` in `src/record_build.rs` now return
`Err("record overflow: …")` if the END-marker write would land
past `record_size`. Prevents the previous silent buffer-overrun.

**Still outstanding**: the `$ATTRIBUTE_LIST` extension-record
mechanism — i.e. the constructive answer to "OK, the attributes
don't fit, so spill some to a satellite record." Not yet exercised
in practice (4096-byte records have ~3700 bytes capacity which is
hard to exhaust with the attributes we currently emit). Suggested
next step when this becomes needed: a negative test creating a file
with a 255-character UTF-16 name + several hard links, plus the
`$ATTRIBUTE_LIST` extension-record builder.

### W2.6 — MFT self-growth

When there are no free MFT records, `$MFT` itself must grow. `$MFT`'s
own `$Bitmap` tracks records; when full, allocate a cluster via the
W2.3 cluster allocator, extend `$MFT`'s `$DATA` runs, add bits to its
`$Bitmap`. This is recursion — careful ordering required to avoid
trying to allocate the bitmap bit for a record that doesn't yet
exist.

---

### W3.2 — `$INDEX_ALLOCATION` B+ tree insert

**Scaffolding already present** (so the cost is the algorithm, not
infrastructure):

- INDX-block decoder + VCN-to-disk-offset translation:
  `src/idx_block.rs:58–138`.
- `$Bitmap` attribute scoped to `$INDEX_ALLOCATION` is loaded and
  parsed: `src/idx_block.rs:28–55`.
- `$UpCase`-table-based `COLLATION_FILE_NAME` comparison is wired
  into the resident-root insert path: `src/index_io.rs:475–698` +
  `src/upcase.rs`. Used live from `write.rs:1001`.
- Cluster allocator + resident-to-non-resident promotion machinery
  used by `$DATA` (`promote_resident_data_to_nonresident_io` etc.)
  is reusable for the index promotion case.

**What's missing**:

- The spill-detection branch in `insert_entry_in_parent_io`
  (`src/write.rs:1002–1047`) — today it scans only allocated INDX
  blocks, gives up with "no INDX block with room ... would need
  B+ tree split / new block allocation", and `create_file` /
  `mkdir` / `rename` refuse parents with `IH_FLAG_HAS_SUBNODES`
  (`src/write.rs:884`, `:1082`, `:1514`).

Two cases to implement:

- **Small dir** (fits in `$INDEX_ROOT`): insert into the resident tree.
  If it no longer fits, promote to `$INDEX_ALLOCATION` (much like
  resident → non-resident promotion done in W2.2).
- **Large dir**: walk B+ tree from root. At each node, binary-search
  by the NTFS collation rule (typically `COLLATION_FILE_NAME`:
  case-insensitive upcase-table comparison; already implemented). At
  leaf:
  - If node has free space: insert, rewrite node, done.
  - If not: split. Allocate a new index node (uses `$Bitmap` attribute
    scoped to `$INDEX_ALLOCATION`), pick a median key, propagate the
    new key up. Recurse on parent if it also splits.

Splitting the root promotes it from resident `$INDEX_ROOT` to
non-resident `$INDEX_ALLOCATION` — same machinery as W2.2.

---

### W3.3 — `$INDEX_ALLOCATION` B+ tree delete

Remove the entry in the leaf, then rebalance on underflow: merge with
sibling or rotate. Merge-and-shrink toward root. If root becomes
resident-sized again, demote back to `$INDEX_ROOT`.

---

### W3 fixtures (planned)

- `ntfs-w3-empty.img` — fresh 32 MiB volume with only root. Tests
  create files into this, verify upstream reads them back.
- `ntfs-w3-deep-tree.img` — pre-built 100k-file dir to stress-test
  B+ tree walks.
- `ntfs-w3-full.img` — volume near capacity, for "allocation failure"
  negative tests.

---

## Phase W5 — journaling (intentionally skipped)

Would require implementing `$LogFile` restart-page synthesis, log
record append, and replay-on-mount. Multiple months; extensive
interoperability risk with Windows. Not blocking any near-term
consumer — `fs_ntfs_fsck` (clear dirty bit + reset `$LogFile` to
`0xFF`) is a sufficient recovery path. Documented as best-effort
crash-safety; we rely on `fs_ntfs_fsck` for post-crash repair
instead.

---

## Testing strategy for new work

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
docs (MS-FSCC, Windows Internals, upstream ntfs) — never at GPL'd NTFS reimplementations.

---

_Last updated: 2026-05-02 — file renamed from `WRITE_PLAN.md` and
slimmed to outstanding work only. Implemented surface lives in
`STATUS.md`._

---

# Beyond the W-plan — outstanding items migrated from NEXT_PLAN.md

These items are not part of the original W0→W4 rollout but were
captured in `NEXT_PLAN.md` while triaging "what's next after W4". They
are reproduced here verbatim (with light editing) so this file is the
single source of truth for outstanding work; `NEXT_PLAN.md` is now a
dormant, fully-commented archive.

Section numbers (§N.M) are preserved from NEXT_PLAN.md to make
cross-referencing existing PRs / commit messages easy.

## Priority legend

- 🔴 **Correctness** — known wrong-behavior paths.
- 🟠 **Scale** — works on small fixtures but breaks on realistic volumes.
- 🟡 **Completeness** — features the spec has that we don't.
- 🟢 **Polish** — won't corrupt anything, but makes the crate nicer.
- 🔵 **Tooling** — things around the crate rather than in it.
- 🧠 **Observability + safety** — invisible until they're not.

---

## 🔴 Correctness — outstanding ABI-break bundle

The two items below break the C ABI; ship them together as a single
breaking change so consumers re-link once. **No on-disk format
change** for either — only the FFI projection widens. (See also
`STATUS.md` §"Documentation cross-check" for the deeper write-up
behind each.)

### §1.3 Timestamp widening to 64-bit + nanoseconds — shipped (2026-05-25)

`FsNtfsAttr::atime` / `mtime` / `ctime` / `crtime` are now split into
an `int64_t *_sec` (UNIX epoch, signed — pre-1970 is negative) and a
`uint32_t *_nsec` (sub-second nanoseconds, always in `[0, 1e9)`).
`ntfs_time_to_unix` returns `(i64, u32)`. ABI break: struct size grew
from 44 to 76 bytes. `facade::Attr` updated in parallel.
Four unit tests cover epoch boundary, sub-second rounding, pre-epoch,
and max-nsec cases.

### §1.4 `fs_ntfs_dirent_t::name[256]` truncation (resolved)

Widened to `name[1024]` via the new `FS_NTFS_DIRENT_NAME_BYTES`
constant in `include/fs_ntfs.h` and `src/lib.rs`. Worst-case UTF-8
encoding of a 255-UTF-16-code-unit NTFS filename is 1020 bytes; a
1024-byte buffer fits content + NUL with margin. Files whose names
exceed the buffer surface with `name_len = FS_NTFS_DIRENT_NAME_BYTES
- 1`; callers can compare against the constant to detect.

**ABI break**: the struct's size and `name` member layout changed in
v0.1.2. Consumers compiled against the old 256-byte layout will
mis-read `name`. Bump SO version on the next release.

---

## 🟠 Scale — beyond W2.6 / W3.2 / W3.3

### §2.4 Large-volume boot-sector paths

**Largely subsumed (2026-05-23 audit)**: the test matrix now
exercises 1 GiB, 4 GiB (cluster 4k + 64k), and 16 GiB (cluster 4k)
volumes alongside the original 32–64 MiB fixtures — see
`test-matrix.json` for `mac-format-large-1gib`,
`mac-format-volume-{4gib,16gib}-*`, and
`mac-format-volume-32mib-cluster-512`.

**64 KiB cluster fix (shipped 2026-05-25)**: `round_trip_64k` was
`#[ignore]` because `src/mft_io.rs` decoded `sectors_per_cluster=0x80`
(128, literal) as the log2 form `1 << (256 - 128) = 1 << 128`, causing
a shift-overflow panic. Fixed by treating values 0x01–0x80 as literals
(unlike `clusters_per_mft_record` which is signed i8 and uses a different
encoding). Two new unit tests added: `parse_boot_spc_128_is_literal_not_log2`
and `parse_boot_advanced_format_4096_bps`. `round_trip_64k` is now active.

**Remaining gap**: full format+remount round-trip at `bytes_per_sector=4096`
(4Kn "native Advanced Format"). `format_filesystem` hardcodes 512-byte
sectors; adding a `bytes_per_sector` parameter touches the boot-sector
builder, backup-boot offset, and all callers.

### §2.5 Dirent eager materialization — shipped (2026-05-25)

`fs_ntfs_dir_open` now uses a lazy phase-based iterator (`LazyDirState`)
with an independent reader per open directory. `FsNtfsDirIter` no longer
pre-loads entries into a `Vec`; entries are streamed from the on-disk
index in `fs_ntfs_dir_next`. Memory is bounded to one `FsNtfsDirent`
scratch buffer regardless of directory size.

---

## 🟡 Completeness — spec features still missing

### §3.1 `chkdsk /scan` exit 0 — shipped (2026-05-24)

`chkdsk DRIVE: /scan` now exits 0 on the first online scan of every
freshly-formatted volume, matching Windows `format.com` behaviour.

**Root cause**: the original `src/logfile-canonical-12k.bin` had LFS
restart page 1 as the authoritative copy (`current_lsn = 0x10634B`
> page 0's `0x104408`). Page 1's active log range `[0x10443C,
0x10634B]` had **zero covering RCRD pages** in the embedded 12 KB
content. On every first mount ntfs.sys entered full log-recovery mode
because it could not find the referenced records; chkdsk's VSS
snapshot captured the volume mid-recovery, returning exit 13.

**Fix**: patched 3 fields in page 1 to make it a stale backup
(`current_lsn`, `oldest_lsn`, `restart_lsn` all set to `0x100000`).
Page 0 becomes authoritative (`cur = 0x104408`); its active range
`[0x100000, 0x104408]` is fully covered by the existing page 2 RCRD
(`lsn = 0x104408`, containing a generic `LfsClientRestart` record
with all-zero payload). ntfs.sys skips recovery entirely; scan1 = 0.

The patch is 24 bytes across three 8-byte fields at offsets
`0x1030`, `0x1070`, `0x1078` in `src/logfile-canonical-12k.bin`. No
USA fixup needed (none of those offsets land on sector-end slots).
`src/mkfs.rs` comment block updated with the full rationale. All 42
test-matrix scenarios pass; no regressions.

**S4 investigation note** (2026-05-23): two S4 attempts on branch
`feature/s4-extend-reparse` (commits 278c676 and 712566a) shipped
`$Reparse` at MFT slot 16 — first as resident `$INDEX_ROOT $R`
only, then as non-resident `$INDEX_ALLOCATION $R` + `$BITMAP $R` +
HAS_SUBNODES root. Both rejected by chkdsk readonly with `Index $R
in file 10 is corrupt` / `Error detected in index $R for file 10`
(where `10` is hex = slot 16). Background byte-diff investigation
showed `$Reparse` lives at slot 26 in the reference, carries
flags = 0x0D, and ships only a resident `$INDEX_ROOT $R` with no
`$INDEX_ALLOCATION` / `$BITMAP`. S4-v3 guidance preserved in
`docs/implementation-plan-secure-and-extend.md`.

### §3.2 NTFS compression (LZNT1)

- **Read — detect-and-error shipped** (2026-05-23): `fs_ntfs_read_file`
  inspects `data_attr.flags()` for `COMPRESSED` (0x0001) or
  `ENCRYPTED` (0x4000) and returns a clear error ("file is
  compressed (LZNT1); decompression not yet supported" /
  "file is encrypted ($EFS); decryption not supported") instead of
  returning raw compressed bytes. Upstream `ntfs` 0.4 still doesn't
  decompress, so a real LZNT1 decoder is what's actually missing
  for read support.
- **Write**: we refuse anything with the compression flag set
  (`src/write.rs:255–266`). Writing new compressed data means
  emitting LZNT1-encoded chunks per `compression_unit`. ~800 LOC —
  big.

### §3.3 Sparse-file explicit management

**Read side**: already works. Sparse `$DATA` runs decode to zero
bytes without IO via the upstream `ntfs` crate's `NtfsReadSeek`
implementation, exercised by `tests/sparse.rs`. No code change
needed; consider adding a one-line doc-comment to `fs_ntfs_read_file`
noting this so callers know holes are transparent.

**Write side** is what's outstanding:
POSIX `fallocate(FALLOC_FL_PUNCH_HOLE)`-style:
`fs_ntfs_punch_hole(image, path, offset, len)` → mark range as
sparse in the data runs, free the clusters. Current truncate can
free tail clusters; hole-punching frees middle clusters.

### §3.4 `$SECURITY_DESCRIPTOR` writes — partial (2026-05-23)

**Minimal version (FILE_ATTRIBUTE_READONLY via
`set_file_attributes`) is shipped** — see `src/write.rs:131-192`
+ the W1.3 entry in `STATUS.md`. Callers can flip the READONLY
bit today.

**`set_security_id` shipped** on branch `feature/set-security-id`
(commit 2a68b09): runtime-created files can be pointed at the
existing `$Secure:$SDS` catalog entry (id 0x100 = the canonical
system-files DACL that mkfs ships). 12 tests cover roundtrip,
72-byte / 48-byte SI form distinction, missing-file errors, the
C-ABI surface (`fs_ntfs_set_security_id` /
`fs_ntfs_read_security_id`), and upstream remount-after-write. The
48-byte v1.x SI form (used by the qemu+Alpine fixture pipeline for
`ntfs-basic.img`) is correctly rejected — it has no `security_id`
field at all.

What's left is **adding new SD entries**: the API today only
retargets a file at an existing entry; it can't append a new SD
to `$Secure:$SDS` / update `$SDH` / `$SII` / grow the security_id
counter. That's substantively more work and the natural follow-up
once a caller hits the limit of "everything maps to id 0x100".

### §3.5 `$OBJECT_ID` write side — shipped (2026-05-23)

16-byte GUID per file, used by DLT (Distributed Link Tracking).
Read side: `fs_ntfs_read_object_id` at `src/write.rs:1425-1463`.
Write side **shipped** on branch `feature/objid-write` (commit
5751fde) and ready to merge: `record_build::build_resident_object_id_attribute`
emits a 16-byte-payload resident `$OBJECT_ID` (attr type 0x40),
and `write::{write,remove}_object_id` follow the existing
reparse-point pattern (find → replace-in-place if present,
otherwise allocate-id + insert-before-end; remove is idempotent).
C ABI: `fs_ntfs_write_object_id` + `fs_ntfs_remove_object_id`.
9 tests in `tests/object_id.rs` cover roundtrip, replace,
remove-idempotence, and the C-ABI surface; all green.

Extended 64-byte form (Birth-volume / Birth-object / Birth-domain
GUIDs per MS-FSCC §2.4.6) **also shipped** on branch
`feature/objid-extended` (commit b4a1344, rebased on top of
feature/objid-write): `build_resident_object_id_attribute_full`,
`write_object_id_extended(_io)`, `read_object_id_extended(_io)`
returning an `ObjectIdExtended { object_id, birth_ids:
Option<...> }` struct. C ABI:
`fs_ntfs_write_object_id_extended` + `fs_ntfs_read_object_id_extended`
with caller-side truncation when out_buf_len < 64. 10 extra tests
cover roundtrip, short-form-no-Birth-IDs, extended-then-short
shrinks correctly, short-read-of-extended, and the C-ABI parallels.

§3.5 is now functionally complete; what's outstanding is the
**parent's $I30 index entry** when a file gets a fresh `$OBJECT_ID`
(no DLT lookup index until that's added; OS-level use cases
needing FSCTL_GET_OBJECT_ID still work because they go through the
MFT record, not the index).

### §3.7 Non-resident named streams + EAs

**Already shipped** via `write_named_stream_io` at
`src/write.rs:1735-1762`: the resident write path catches
"insufficient space" / "exceeds" / "no room" errors, deletes the
resident attribute, and calls `promote_attribute_to_nonresident_io`
(`src/write.rs:1741-1758`). Single-pass native synthesis (build
non-resident directly, skip the resident-then-promote round-trip)
is an optional optimisation, not blocking.

### §3.8 WOF (Windows Overlay Filter) decompression

Modern Windows 10/11 volumes have most of `C:\Windows\` stored as
empty unnamed `$DATA` + `IO_REPARSE_TAG_WOF` (0x80000017) +
`WofCompressedData` ADS. Without WOF *decompression*, reading
`notepad.exe` from such a volume would return 0 bytes.

**Detect-and-error shipped** (`src/lib.rs:fs_ntfs_read_file`):
`fs_ntfs_read_file` now walks the file's attributes, finds any
`$REPARSE_POINT`, reads the 4-byte tag, and returns a clear error
("file is WOF-compressed (IO_REPARSE_TAG_WOF); decompression not
yet supported") when the tag is 0x80000017. No more silent zero
returns.

**Real decompression** is still outstanding: requires XPRESS4K/8K/16K
+ LZX decoding of the `WofCompressedData` ADS. Third-party crate
(`ms-compress`) does it; bindings would be ~200 LOC. Listed as the
biggest single read-correctness gap in STATUS.md cross-check
"#### WOF (Windows Overlay Filter) compression not supported".

### §3.9 Case-sensitive directory flag — primitive shipped (2026-05-23), wire-through pending

`FILE_CASE_SENSITIVE_DIR` is the per-directory case-sensitive flag
(Win10 1803+; WSL / Docker-Desktop set it on container-image
directories). Inside such a directory `foo.txt` and `FOO.TXT` are
distinct files.

**Shipped** on branch `feature/case-sensitive-dir` (commit 6f1d09b):
`index_io::compare_names_ordinal(a: &[u16], b: &[u16]) -> Ordering`,
the byte-for-byte UTF-16 comparator a case-sensitive directory
should use. 4 unit tests cover case distinction, exact-bytes
equality, prefix ordering, and non-ASCII code-point ordering.

**Wire-through still needed**:

1. Pin the actual bit position of FILE_ATTRIBUTE_CASE_SENSITIVE_DIR
   within file_attributes. Investigation 2026-05-23 confirmed
   **the value is contested**:
   * This repo's docs claim `0x00010000`, which matches Windows
     SDK `winnt.h`'s `FILE_ATTRIBUTE_VIRTUAL` ("reserved for
     system use") — interesting overlap but no Microsoft source
     documents the per-directory case-sensitivity flag at that
     bit.
   * Microsoft Learn / MS-FSCC §2.6 publishes file-attribute
     constants up to `0x00400000` and **does not list a
     case-sensitive-dir bit** at any value.
   * Third-party reverse-engineering notes circulate
     `0x00010000`, `0x80000000`, and a separate `$STANDARD_INFORMATION`
     extension slot (not file_attributes at all).
   * Microsoft WSL docs document the *user-facing* feature
     (`fsutil file setCaseSensitiveInfo`) but deliberately don't
     disclose the on-disk encoding.

   The only authoritative path is a **byte-diff against a real
   WSL or Docker-Desktop NTFS volume**: create a directory with
   `fsutil file setCaseSensitiveInfo <path> enable`, then compare
   its `$FILE_NAME.file_attributes` and `$STANDARD_INFORMATION.file_attributes`
   bytes against a sibling normal directory to identify the bit.
2. Thread `case_sensitive: bool` through `find_index_entry` /
   `insert_entry_into_index_root` / the INDX-block variants.
3. At every lookup site, read the parent directory's flag and pick
   the right comparator.

Today the `compare_names` (case-insensitive) path is used
unconditionally; existing matrix scenarios don't carry the flag,
so the lack of wire-through has no observable effect on what we
ship. The primitive lets the wire-through land in a single small
follow-up PR once #1 is settled.

### §3.10 Runtime volume-label writer — shipped (2026-05-23)

Symmetric to mkfs's `-L LABEL` flag: `set_volume_label(image,
&str)` / `fs_ntfs_set_volume_label(image, label)` updates
`$Volume`'s `$VOLUME_NAME` (attr 0x60) at MFT slot 3. Empty
string / NULL removes the attribute entirely. `read_volume_label`
+ `fs_ntfs_read_volume_label` is the symmetric reader. Hard-cap at
32 UTF-16 code units per Microsoft tools' convention; longer
labels rejected with `Err("volume label too long: …")`.

Shipped on `feature/set-volume-label` (commit c1eb03a). 11 tests
cover roundtrip, empty-removes, max-length, too-long rejection,
mixed-Unicode (BMP + non-BMP), upstream remount after write and
after remove, plus C-ABI parallels. No on-disk format change
beyond what mkfs already produces.

### §3.11 Diagnostic-helper read APIs — shipped (2026-05-23, extended 2026-05-24)

A family of read-only helpers added during these sessions to
support byte-diff investigations (S4 `$Reparse`, case-sensitive
flag bit-position research, multi-namespace files, etc.). All
ship on small focused branches; tests live alongside.

| API | Branch / commit | What it returns |
|---|---|---|
| `read_attributes` / `describe_attributes` | `feature/read-attribute-list` c54d817 | List of `AttrDescription { type_code, type_name, name, attr_offset, attr_length, is_resident, value_length }` for every attribute on a file's MFT record |
| `read_file_names` | `feature/read-file-names` fd03f26 | One `FileNameRecord { namespace, name, parent_reference, file_attributes }` per `$FILE_NAME` on the file |
| `read_security_id` | `feature/set-security-id` 2a68b09 (shipped alongside the writer) | `Option<u32>` — `None` for the 48-byte v1.x SI form, `Some(id)` for 72-byte v3.x |
| `read_object_id_extended` | `feature/objid-extended` b4a1344 | `Option<ObjectIdExtended>` with full 64-byte form (object_id + Birth GUIDs) when present |
| `read_volume_label` | `feature/set-volume-label` c1eb03a | `String` (UTF-8); empty if `$VOLUME_NAME` absent |
| `fs_ntfs_get_volume_info_v2` | `feature/volume-info-v2` 7d64f3f | `FsNtfsVolumeInfoV2` adding `volume_flags` / `is_dirty` / `mft_record_size` / `bytes_per_sector` on top of v1 |
| `read_reparse_point` | `feature/read-reparse-point` bd977e3 | `Option<ReparsePoint { reparse_tag, data }>` — raw payload for any reparse type, complement to readlink-only |
| `list_named_streams` | `feature/list-named-streams` 5b45fb1 | `Vec<String>` of named `$DATA` attribute names (ADS), excluding the unnamed primary |
| `list_ea_keys` | `feature/list-ea-keys` 6e2edaa | `Vec<Vec<u8>>` of EA names only (skips values up to 64KB each — cheap enumeration) |
| `read_si_full` | `feature/read-si-full` 7ed5c60 | `StandardInformationFull { timestamps, file_attributes, ..., v3: Option<{owner_id, security_id, quota, usn}> }` — every MS-FSCC §2.4.2 field |

These don't ship as a unified "diagnostics module" — each lives in
the natural place (write.rs for the path-based functions, attr_io
for the attribute walker). They share the same pattern: open
read-only, resolve path → record number, parse the attribute, return
a structured Rust value. Most have C-ABI wrappers for use from
external tools.

Writer-side complement also added in the staging-2 stack:
`fs_ntfs_set_object_id_extended_h` (976ce03) — handle-based sibling
of `fs_ntfs_write_object_id_extended`, for callers that already hold
an open filesystem handle and don't want a per-call image reopen.

---

## 🟢 Polish — small but user-visible

### §4.6 Diagnostic counter for skipped index entries (resolved)

`fs_ntfs_dir_open` now records every silently-skipped entry
(malformed rows, undecodable keys) in a `skipped_count: u64` field
on `FsNtfsDirIter`. Callers query it via the new
`fs_ntfs_dir_skipped(iter)` accessor — returns the count, or -1 on
a NULL iterator. Skip-on-error behaviour is unchanged so a single
bad entry still doesn't abort the listing.

DOS-namespace dedup skips do NOT count (intentional dedup, not error).
A non-zero skipped count means the listing is incomplete.

### §4.7 Header doc on `fs_ntfs_mount` referencing dirty-volume probe (resolved)

The doc comments on `fs_ntfs_mount` and `fs_ntfs_mount_with_callbacks`
in `include/fs_ntfs.h` now describe the dirty-volume contract and
recommend calling `fs_ntfs_is_dirty` (or
`fs_ntfs_is_dirty_with_callbacks`) post-mount to detect possibly
stale state. The driver still parses dirty volumes silently — the
auto-warn / auto-refuse decision belongs to the caller per the
quiet-by-default contract.

---

## 🔵 Tooling — around the crate

### §5.2 Fuzz harness (resolved)

`fuzz/` subcrate ships three `libfuzzer-sys` targets covering the
crate's three byte-decoders most likely to panic on a crafted image:

  - `decode_runs` — wraps `fs_ntfs::data_runs::decode_runs`
  - `decode_eas` — wraps `fs_ntfs::ea_io::decode`
  - `iter_attributes` — drains `fs_ntfs::attr_io::iter_attributes`

Run with `cargo +nightly fuzz run <target>` (after
`cargo install cargo-fuzz`). Each returns ok on Err — we're hunting
panics, OOB reads, and infinite loops, not Result::Err shapes.

`fuzz/target` and `fuzz/corpus` are gitignored. Future work: store
seed corpora alongside as `corpus/<target>/{seed1,seed2,…}` once
crash-replicating inputs surface.

### §5.3 Criterion benchmarks (resolved for byte-decoders)

`benches/byte_decoders.rs` covers the three byte-decoders the fuzz
harness already targets (`data_runs::decode_runs`,
`ea_io::decode`, `attr_io::iter_attributes`). Each input is
hand-constructed in-memory to exercise a realistic shape — single
run, eight-run zigzag, sparse-then-data; single small EA, sixteen
short EAs; minimal three-attr MFT record.

Run with `cargo bench --bench byte_decoders`. Reports under
`target/criterion/<group>/<id>/report/index.html`.

Future: `bench/write_at_1gb.rs` / `bench/create_many.rs` would
exercise the higher-level mutation paths but need a writable
in-memory `BlockIo` adapter that doesn't ship yet — added when the
write surface stabilises further.

### §5.4 CI matrix expansion (partly resolved)

`test` job now runs on `ubuntu-latest` AND `macos-latest`, with
`fail-fast: false` so one OS failing doesn't kill the other. The
cargo-deny step is gated to ubuntu only since licence checks don't
vary by OS.

Still pending:

- **MSRV check.** Pin a minimum Rust version in `rust-toolchain.toml`
  and add a separate `runs-on: ubuntu-latest` job that builds with
  that version. Catches accidental MSRV bumps when a new clippy lint
  or std API gets used.
- **Windows-runner test build.** `test` currently only runs on
  Linux + macOS; a `cargo test --release --lib` on `windows-latest`
  would catch path-separator / file-mode regressions before they
  hit the validate-mkfs-windows chkdsk job.

### §5.5 Sanitizer runs (resolved)

A nightly-only `asan` job runs `cargo +nightly test --release --lib`
with `RUSTFLAGS="-Zsanitizer=address"` against
`x86_64-unknown-linux-gnu`. Marked `continue-on-error: true` so
nightly-toolchain breakage doesn't block stable PRs but the smoke
signal is still recorded.

Catches OOB reads/writes in the raw-byte helpers (mft_io / data_runs
/ attr_io / ea_io) that pure cargo test on the test fixtures
wouldn't otherwise surface.

Future: also wire Miri once we have tests that don't depend on
real-FS access (Miri can't drive `std::fs::File` against on-disk
images).

### §5.7 Release pipeline (resolved)

Tag-driven publication of `rust-ntfs` binaries lands via
`.github/workflows/release.yml`, added in `0d89b60`. Six target
triples (aarch64/x86_64-apple-darwin, x86_64/aarch64-unknown-linux-gnu,
x86_64-pc-windows-msvc, aarch64-pc-windows-gnullvm) build on their
native runners and attach tar.gz / zip + SHA-256 checksums to the
GitHub Release. Pushing a `v*` tag (or running the workflow with a
tag input) cuts a release.

End-to-end verification still pending — the first real tag push
will exercise the workflow against actual GitHub runners. Until then,
this is shipped-but-unverified.

### §5.8 `cargo-deny` / licence hygiene (resolved)

Configured in `deny.toml`; CI step `cargo-deny check` runs in
`.github/workflows/ci.yml` on every push / PR. Allowlist:
MIT / Apache-2.0 (+LLVM exception) / BSD-2/3 / ISC / Unicode /
Zlib / CC0 / 0BSD. Anything outside fails the build. Yanked
versions and unknown registries also rejected. Pairs with the
project-wide "no GPL/LGPL/AGPL" rule.

### §5.9 Test-matrix Stage A — 2 GiB raw-write cap (resolved in v2 harness)

**Resolved (2026-05-23 audit)**: the v2 harness
(`scripts/v2/_lib.ps1`) replaced `[System.IO.File]::ReadAllBytes`
with a 16 MiB chunked `FileStream.Write()` loop. Tier-1 4 GiB +
16 GiB scenarios (`mac-format-volume-4gib-cluster-{4k,64k}`,
`mac-format-volume-16gib-cluster-4k`) now stream through that path
and pass in the 42-scenario matrix. The original `scripts/run-scenario.ps1`
(Stage A 2 GiB cap) is no longer used; this entry is kept for
audit trail of how the limit was actually lifted.

### §5.10 Test-matrix — op-by-op chkdsk interleaving

**Status**: simplification; `scripts/run-scenario.ps1` Stage B2
applies all `fixture_files` in one batch BEFORE the Stage E chkdsk
pass. So an `operation_sequence` like

  `mac:format -> win:chkdsk -> win:write(F1) -> win:chkdsk`

collapses in practice to "format, mount, apply F1, run chkdsk
once" — the pre-write chkdsk is silently dropped.

**Why it matters**:

- Loses the ability to assert "the format is clean BEFORE the
  write, AND clean AFTER" in a single scenario. Today you'd need
  two scenarios (one without the write, one with) to cover both
  states.
- Some scenarios encode a meaningful interleave (e.g. a chkdsk
  between a write and a delete to verify the bitmap is consistent
  at every step). Those collapse to a single end-of-run chkdsk.

**Productive next moves**:

1. Have `tests/matrix.rs` parse `operation_sequence` into a typed
   list of step tokens (`Format`, `Write(path,data)`, `Chkdsk`,
   `Delete(path)`, `Mount`, `Dismount`, …) and serialise the
   typed list as a "step plan" JSON for the PS script.
2. PS reads the step plan and executes each step in order, writing
   per-step diag files (`step01-chkdsk-readonly-exit.txt`,
   `step02-write.txt`, …). The verdict becomes "every chkdsk in
   the plan exits clean", not "the single trailing chkdsk exits
   clean".
3. Keeps backward compatibility with today's flat
   `fixture_files` shape by treating it as syntactic sugar for
   "single Write step before the trailing chkdsk".

**Effort estimate**: medium (~1 day). Touches the PS step
machinery + Rust step-plan generator + a couple of new scenarios
that prove the per-step verdict shape.

### §5.11 Test-matrix `chkdsk /F` repair-lane verdict (resolved)

PS Stage G now emits `FIX_EXIT=<n>` and `POSTFIX_SCAN_EXIT=<n>`
markers when Stage E2 ran. The matrix runner parses them and
applies one of three shapes per scenario, declared via the new
optional `verdict_shape` field in `test-matrix.json`:

| Shape | Pass condition |
|---|---|
| `clean` (default) | `ro==0` AND `scan` ∈ {0, 11, 13}. Same as before. |
| `repair-ok` | `clean` passes OR `FIX_EXIT==0` AND `POSTFIX_SCAN_EXIT` ∈ {0, 11, 13}. |
| `repair-required` | `FIX_EXIT==0` AND `POSTFIX_SCAN_EXIT==0`. /F must have run AND repaired AND post-/F /scan must be perfectly clean. |

Tier-3 dirty-volume scenarios (`mac-format-set-dirty-win-chkdsk`
and friends) are tagged `verdict_shape: "repair-required"` so they
fail unless chkdsk genuinely detects + repairs the dirty bit.
Existing scenarios continue to use the default `clean` shape with
no behavioural change.

---

## 🧠 Observability + safety — invisible until they're not

### §6.1 Transactional semantics across multiple records

`create_file` touches: MFT record for the new file + parent record +
`$MFT:$Bitmap`. Each is individually `fsync`'d, but there's no
multi-record atomicity. A crash mid-create can leave:

- MFT record populated, bitmap bit set, no index entry — leaked
  allocation (space wasted, no correctness issue).
- MFT record populated, no bitmap bit — allocator may reuse the
  slot, overwriting the record.

**Current ordering** (verified at `src/write.rs:900-966`):
1. `mft_bitmap::allocate_io` — bitmap bit set first.
2. `update_mft_record_io` — MFT record body + sync.
3. `insert_entry_in_parent_io` — parent index entry + sync.

A crash after step 1 leaves a "claimed but empty" slot (free bit
reused on next allocate; the unfilled bytes get overwritten). A
crash after step 2 leaves a leaked allocation, recoverable by
`fs_ntfs_fsck`. The current ordering is the **stricter-ordering**
option from "fix options" below; it's already in place.

**Fix options** (for stronger guarantees than the current ordering):

- **Intention log**: write a tiny "I'm about to X" record in a
  dedicated scratch attribute, replay on mount. Essentially
  mini-journal. ~500 LOC.
- Phase W5 / `$LogFile` writeback + replay is the full-journaling
  alternative; intentionally skipped per the original W5 decision.

### §6.4 Tracing hooks (resolved at lifecycle layer)

`log = "0.4"` added as a dep — the de-facto Rust facade. Consumers
install whichever subscriber they want; the crate stays quiet by
default until one is set.

Instrumented today (info-level):

  - `fs_ntfs_mount` — emits `mount path=<p>` on success.
  - `fs_ntfs_umount` — emits `umount handle=<ptr>` on each free.
  - `fsck::set_dirty` / `clear_dirty` / `fsck` — emit one entry per
    call with the path; `fsck` also emits a done-line with the
    report.

Future: trace-level events at attribute read / cluster alloc /
bitmap flip, gated behind a feature flag so the trace-call overhead
doesn't sit in the hot path of consumers that don't subscribe to
trace level. Today's lifecycle-only instrumentation is enough for
"why did mount/fsck happen?" debugging on FSKit reports.

### §6.5 `Send` safety contract for `CallbackReader` context pointer (resolved)

The `unsafe impl Send for CallbackReader` block in `src/lib.rs` now
spells out:

  - **What FSKit's per-volume callback serialisation actually
    guarantees** (mutex-free aliasing of `position` from inside
    `read_fn`).
  - **What it does NOT guarantee** that callers MUST arrange:
    thread-confined contexts (`@MainActor`-bound
    `FSBlockDeviceResource`, etc.) need a Sendable wrapper because
    fs_ntfs may drop the handle on any thread; and the read
    callback must not re-enter fs_ntfs against the same handle.

**Fix**: expand the Safety comment to spell out the contract the
Swift caller must uphold for their `context` pointer (Send-safe,
drop-on-any-thread). ~10 lines of Rust prose. No code change. The
"add an `fs_ntfs_drop_on_thread(handle)` helper" suggestion from
the original review is **deferred** — docs-only is the chosen fix.
Migrated from code-review-2026-04-19 §3.

### §6.6 Concurrency contract on `update_mft_record` (resolved)

`src/mft_io.rs` now carries a top-level "Concurrency contract"
doc-block stating:

  - `update_mft_record` is NOT safe under concurrent writers to the
    same image (the read-mutate-write window can be torn).
  - Single-process, single-thread usage is safe (the crate doesn't
    spawn threads internally).
  - Multi-process or external writers (Windows mounting the same
    volume, an upstream NTFS driver, a second fs-ntfs caller) is UB
    — quiesce the image first.
  - Advisory file locking is deliberately not added (can't prevent
    external concurrency anyway).

---

## Sequencing (revised post-W4)

Original NEXT_PLAN.md sequencing is preserved here for the still-
outstanding work; steps that were completed have been dropped.

1. **ABI-break bundle** (§1.3 timestamp widening + §1.4 name buffer +
   any extended `VolumeStats` fields): one coordinated breaking
   change so consumers re-link once.
2. **MFT growth + B+ tree split** (W2.6 + W3.2/3.3 above): the
   single biggest engineering item remaining; gates "infinite
   creates" scale.
3. **WOF decompression** (§3.8): closes the modern-Windows
   read-correctness gap.
4. **Lazy dir iterator** (§2.5) + **large-volume fixtures** (§2.4):
   unblocks 1M-entry directories and surfaces cluster/sector
   off-by-ones.
5. **Tooling backlog**: §5.2 cargo-fuzz, §5.3 criterion benches,
   §5.4 macOS CI, §5.5 sanitizers, §5.7 release pipeline, §5.8
   cargo-deny.
6. **Completeness polish**: §3.3 punch-hole, §3.5 `$OBJECT_ID`
   write, §3.7 non-resident named streams + EAs, §3.9 case-sensitive
   flag, §3.4 minimal `$SECURITY_DESCRIPTOR`.
7. **Docs/diagnostics backlog from 2026-04-19 code review** (small,
   can interleave with anything else): §4.6 skipped-entry counter,
   §4.7 dirty-mount header doc, §6.5 `Send` safety contract, §6.6
   `update_mft_record` concurrency note. Total ~50 lines of code/
   prose across four sites.

Beyond that, §6.1 transactional semantics, §6.2 / W5 `$LogFile`,
§6.4 tracing — i.e. observability + journaling — is where a
"production-ready NTFS driver" graduates from "good enough for an
FSKit extension" to "ready for high-availability use". That's a
separate project, easily a person-year.

---

## Invariants & rejected approaches

These hold across the whole crate; future agents should consult
before proposing "defensive" patches that look reasonable in
isolation. (Captured during the 2026-04-19 code-review triage.)

### Invariants

- **NTFS on-disk format is sacred.** Any fix must preserve a valid
  on-disk NTFS layout. ABI/FFI reshaping (struct widths, header
  contents, error returns) is fine; the bytes the kernel sees on
  the volume are not. FILETIME stays u64, file-name length max
  stays 255 UTF-16 code units, etc., regardless of what the FFI
  projection looks like.
- **fs-ntfs operates on an image file, not a raw block device.**
  Reads/writes go through `std::fs::File` (or the callback adapter
  bridging to one). A bogus byte offset surfaces as `read_exact`
  returning `UnexpectedEof`, not as out-of-bounds memory access.
  This shapes the threat model: "memory safety" claims about
  unchecked offsets in the MFT-walking helpers don't hold —
  failure modes are confusing error messages, not exploitable
  reads. Bounds-checking belongs at public-API entry points (which
  already resolve by path), not at the math-helper level.
- **`cstr_to_path` is the standard for path arguments; raw
  `CStr::from_ptr` is intentional for byte-string arguments.**
  Paths (61 sites in `src/lib.rs`) all funnel through
  `cstr_to_path`; byte-string args (EA names, stream names) bypass
  it deliberately because they aren't paths. An apparent
  "inconsistency" between the two patterns is not a bug.

### Rejected approaches (do not re-propose)

- **`checked_sub` / defensive arithmetic at `src/write.rs:492` in
  `grow_nonresident`.** The preceding guard at `src/write.rs:477`
  rejects `new_size <= current_len` and returns early, so the
  `(new_size - 1) / cluster_size` line below cannot underflow.
  Adding `checked_sub` here is defensive noise that obscures the
  invariant.
- **Bounds-checking inside `mft_record_offset`
  (`src/mft_io.rs:89-91`).** It's a pure math helper. Querying
  `$MFT.value_length` per call is expensive; the failure mode of a
  bogus record number is "read_exact returns EOF", not "memory
  unsafety". Validate at the public-API layer, not in the helper.
- **Advisory file locking around `update_mft_record`.** External
  concurrent writers (Windows, NTFS driver, a second fs-ntfs caller)
  cannot be prevented by advisory locks even if we held them, so
  the lock would only catch in-process races we already don't
  produce. The chosen alternative (a documented UB contract) is
  §6.6 above.
- **Auto-warning on `fs_ntfs_mount` when the volume is dirty.**
  Would change the quiet-by-default contract FSKit relies on. The
  driver does parse dirty volumes (may return stale data, doesn't
  panic). Discoverability gap is closed by §4.7 instead — a header
  doc-comment pointing at `fs_ntfs_is_dirty`.
- **`fs_ntfs_drop_on_thread(handle)` helper for thread-confined
  Swift contexts.** Considered during §6.5 triage and deferred —
  the contract documentation is sufficient until a confined-context
  consumer actually asks for it.

---

## What stays out of scope

- **Write support for encrypted files** (EFS). Requires Windows-only
  crypto stack. Refuse.
- **Upstream `ntfs` 0.5+ migration.** Keep pinned at 0.4 until
  upstream changes force otherwise.
- **Quota management.** Rare in practice; refuse for now.
- **32-bit target support.** FSKit is 64-bit only.
