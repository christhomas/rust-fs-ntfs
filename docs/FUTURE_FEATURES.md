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

### W2.6 — MFT self-growth

When there are no free MFT records, `$MFT` itself must grow. `$MFT`'s
own `$Bitmap` tracks records; when full, allocate a cluster via the
W2.3 cluster allocator, extend `$MFT`'s `$DATA` runs, add bits to its
`$Bitmap`. This is recursion — careful ordering required to avoid
trying to allocate the bitmap bit for a record that doesn't yet
exist.

---

### W3.2 — `$INDEX_ALLOCATION` B+ tree insert

Two cases:

- **Small dir** (fits in `$INDEX_ROOT`): insert into the resident tree.
  If it no longer fits, promote to `$INDEX_ALLOCATION` (much like
  resident → non-resident promotion done in W2.2).
- **Large dir**: walk B+ tree from root. At each node, binary-search
  by the NTFS collation rule (typically `COLLATION_FILE_NAME`:
  case-insensitive upcase-table comparison). At leaf:
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

### §1.3 Timestamp widening to 64-bit + nanoseconds

**Today**: `FsNtfsAttr::atime` / `mtime` / `ctime` / `crtime` are
`uint32_t` UNIX-epoch seconds. `ntfs_time_to_unix` does
`.saturating_sub(EPOCH_DIFF) as u32`.

**Problem**: pre-1970 timestamps clamp to 0, post-2038 timestamps
wrap, sub-second precision is dropped. Silently wrong for backup
metadata, SMB peers, archive volumes.

**Fix**: widen to `int64_t seconds + uint32_t nsec`. ~50 LOC.
Convert as `(ts / 10_000_000) as i64 - EPOCH_DIFF` +
`((ts % 10_000_000) * 100) as u32`. FILETIME on disk is u64
(100 ns intervals since 1601-01-01 UTC, representable to year 30828);
only the FFI projection is widening.

### §1.4 `fs_ntfs_dirent_t::name[256]` truncation

**Today**: fixed 256-byte buffer, `min(name_bytes.len(), 255)`.

**Problem**: NTFS filenames are 255 UTF-16 code units. UTF-8
encoding can be up to 4 bytes/unit → 1020 bytes max. A single emoji
or long CJK name gets silently truncated; subsequent `fs_ntfs_stat`
on the truncated name fails with ENOENT.

**Fix**: widen to `name[1024]`. ~5 LOC.

---

## 🟠 Scale — beyond W2.6 / W3.2 / W3.3

### §2.4 Large-volume boot-sector paths

**Today**: all fixtures are 16–64 MiB. Cluster size 4 KiB with
512-byte sectors. Well-tested only for this exact shape.

**Fix**: fixture matrix — add 512-MiB + 2-GiB fixtures; cluster
sizes 512 / 4096 / 65536; sector sizes 512 / 4096 (Advanced
Format). Probably catches 2–3 subtle off-by-ones. ~200 LOC of
`_vm-builder` changes plus fresh tests.

### §2.5 Dirent eager materialization

**Today**: `fs_ntfs_dir_open` reads every entry into a `Vec`
(`src/lib.rs` ~825-862).

**Problem**: 270 MB on a 1M-entry directory. FSKit OOMs. Eager
materialization also blocks the first `readdir` by seconds and
visibly stalls Finder on large dirs. (Cross-referenced in STATUS.md
"Phase 3 #9" and "#### Eager directory materialization — unbounded
memory".)

**Fix**: lazy iterator holding upstream's `NtfsIndexEntries` +
reader reference. Lifetime plumbing is awkward but bounded. ~100
LOC.

---

## 🟡 Completeness — spec features still missing

### §3.1 `chkdsk /scan` exit 13 ceiling — pin down the differentiator

**Status**: known gap; matrix tests currently accept `scan == 0 | 11 | 13`
in `tests/matrix.rs` to bypass it. See the `TODO(/scan-13-ceiling)`
comment there. Once this is fixed, tighten back to `scan == 0`.

**What's known** (full investigation in
[`docs/overnight-findings.md`](./overnight-findings.md) iter G):

- All 12 matrix scenarios produce volumes that pass `chkdsk readonly`
  with exit 0 ("found no problems") and `chkdsk /F` with exit 0
  ("no problems found"). The volume mounts as NTFS, label and size
  are correct, files can be created and read back. Functionally
  sound.
- `chkdsk /scan` consistently returns 13 ("errors queued for offline
  repair") on our volumes but exits 0 on Microsoft `format.com`'s
  output of the same scenario, despite both volumes being byte-similar
  in every checked structural field (BPB, $VOLUME_INFORMATION
  major/minor/flags, $STD_INFO size, $FILE_NAME content, $SECURITY_DESCRIPTOR,
  $LogFile RSTR pages, $AttrDef bytes, root $I30 entries, placeholder
  records 11-15 with link_count=0).
- Running `chkdsk /F` on our volume modifies it (drops $SD on most
  records, transforms $Extend into a real directory, adds $TXF_DATA
  to root, adds $O/$Q view indexes on slot 9) — *but reference's
  volume already passes /scan without those modifications*.
- Hypotheses tested and ruled out: $VOLUME_INFORMATION version
  (1.2 vs 3.1), flags (0x0084 vs 0x0080 vs 0x0085), 72-byte $STD_INFO,
  bootstrap bytes, 256-record initial MFT, SD_ROOT_DIR last-byte
  typo, link_count=0 on placeholders, $Extend as real directory,
  $BadClus off-by-one, dirty-bit set.

**Productive next moves** (not yet attempted):

1. Capture every disk read `chkdsk /scan` performs against our volume
   via Windows Procmon on the test VM, correlate with what /scan does
   against the reference. The reads /scan does that readonly doesn't
   pinpoint exactly which bytes the validator keys on.
2. Time-bisect: Mount-DiskImage with `-NoDriveLetter`, manually run
   `Set-Disk -IsOffline $false`, then assign letter — different
   sequencing might shift ntfs.sys's first-mount-state behaviour.
3. Implement the full `$RmMetadata` / `$Repair` hierarchy under
   `$Extend` even though reference doesn't have it — it may be a
   "creation marker" /scan looks for. (Lower confidence given ref
   doesn't have it either.)

**Effort estimate**: unknown (the hypothesis space we've ruled out
is wide; the remaining surface is deep-Windows-internals).

### §3.2 NTFS compression (LZNT1) write

- **Read**: already works via upstream.
- **Write**: we refuse anything with the compression flag set.
  Writing new compressed data means emitting LZNT1-encoded chunks
  per `compression_unit`. ~800 LOC — big.

### §3.3 Sparse-file explicit management

POSIX `fallocate(FALLOC_FL_PUNCH_HOLE)`-style:
`fs_ntfs_punch_hole(image, path, offset, len)` → mark range as
sparse in the data runs, free the clusters. Current truncate can
free tail clusters; hole-punching frees middle clusters.

### §3.4 `$SECURITY_DESCRIPTOR` writes

`$Secure` / `$SDS` lookup is a separate rabbit hole (stream of SIDs
+ ACEs, shared across files by `security_id` in SI). Minimal
version: let the caller OR bits into SI's `file_attributes` (we
already do READONLY via `set_file_attributes`); punt on full ACL
support.

### §3.5 `$OBJECT_ID` write side

16-byte GUID per file, used by DLT (Distributed Link Tracking).
Read side already shipped (`fs_ntfs_read_object_id` +
`Filesystem::object_id`). Write/builder side — creating a
`$OBJECT_ID` attribute on a file that has none — is still pending.
A few lines of attribute-builder code.

### §3.7 Non-resident named streams + EAs

For the rare but possible case of a multi-MiB alternate data
stream or a huge EA payload. Same mechanics as the generic
non-resident promotion already shipped (W2.3 / former §2.3).
Tracked alongside "W4 polish" above.

### §3.8 WOF (Windows Overlay Filter) decompression

Modern Windows 10/11 volumes have most of `C:\Windows\` stored as
empty unnamed `$DATA` + `IO_REPARSE_TAG_WOF` + `WofCompressedData`
ADS. Without WOF support, reading `notepad.exe` returns 0 bytes.
**Silent data loss on every modern volume.**

Requires XPRESS4K/8K/16K + LZX decompression. Third-party crate
(`ms-compress`) does it; bindings would be ~200 LOC. Listed as the
biggest single read-correctness gap in STATUS.md cross-check
"#### WOF (Windows Overlay Filter) compression not supported".

### §3.9 Case-sensitive directory flag

`FILE_CASE_SENSITIVE_DIR` (WSL / Docker-Desktop). Our writes never
set it; our reads never check it. On a dev volume with
case-sensitive subdirs, we collapse `foo.txt` and `FOO.TXT` to
whichever the B-tree finds first. (STATUS.md cross-check
"#### Per-directory case-sensitivity flag ignored".)

---

## 🟢 Polish — small but user-visible

### §4.6 Diagnostic counter for skipped index entries

**Today**: `fs_ntfs_dir_open` / `_next` walks `NtfsIndexEntries` and
silently `continue`s on `Err` (`src/lib.rs` ~709-717). A malformed
index entry on a dirty volume disappears from the listing with no
trace.

**Problem**: skip-on-error is the right default (one bad entry
shouldn't make a directory unreadable during FSKit enumeration), but
the caller has no way to tell whether they got a complete listing.
Surfaces as "the file is missing" with no diagnostic trail.

**Fix**: keep the skip behavior, but add `skipped_count: u64` (and
optionally `last_skip_reason: String`) to the iterator struct.
Surface via a new `fs_ntfs_dir_skipped(iter)` accessor. ~30 LOC. No
NTFS impact. Migrated from code-review-2026-04-19 §5.

### §4.7 Header doc on `fs_ntfs_mount` referencing dirty-volume probe

**Today**: `fs_ntfs_mount` (`include/fs_ntfs.h:108-112`) is silent
about how to handle dirty volumes. The standalone `fs_ntfs_is_dirty`
probe shipped (former §4.4) but callers don't know to invoke it.

**Problem**: the driver parses dirty volumes — it may return stale
data but doesn't panic. Auto-warning on mount would change the
quiet-by-default contract FSKit relies on. The remaining gap is
discoverability.

**Fix**: extend the doc comment on `fs_ntfs_mount` (and the
`_with_callbacks` variant) to recommend calling `fs_ntfs_is_dirty`
post-mount and dispatching appropriately. ~10 lines of header prose.
No code change. Migrated from code-review-2026-04-19 §13.

---

## 🔵 Tooling — around the crate

### §5.2 Fuzz harness

`cargo-fuzz` target for `data_runs::decode_runs`, `ea_io::decode`,
`attr_io::iter_attributes`. All three take raw bytes and are the
most likely panic sources on a crafted image. Finds off-by-ones
fast.

### §5.3 Criterion benchmarks

`bench/write_at_1gb.rs`, `bench/create_many.rs`. Detects performance
regressions as we refactor.

### §5.4 CI matrix expansion

Today: Ubuntu-latest only (test job). Windows chkdsk validation
already wired up (tag-/manual-triggered). Still missing:
**macOS-latest** (NTFS driver binary compat differs) and an MSRV /
stable Rust matrix. ~30 lines of `ci.yml` plus making sure the
fixture-generation pipeline runs on macOS.

### §5.5 Sanitizer runs

`cargo +nightly test -Zsanitizer=address`. The crate's raw-byte
buffer manipulations are the most likely spot for OOB reads/writes.

### §5.7 Release pipeline verification

Tag-driven publication of `libfs_ntfs-vX.Y.Z-macos-universal.tar.gz`,
cloned from fs-ext4 — a workflow may exist but has never been
verified end-to-end. (STATUS.md Phase 4 #16 also flags that
`.github/workflows/` currently only contains `ci.yml` — confirm
whether a release.yml is intended to live here.)

### §5.8 `cargo-deny` / licence hygiene

Since the crate is strict about no-GPL (per project policy),
add a `cargo-deny` step in CI that asserts dependency licenses
remain MIT / Apache / BSD. Catches accidental regressions. Pairs
with the project-wide "no GPL/LGPL/AGPL" rule.

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

**Fix options**:

- **Stricter ordering**: MFT record → bitmap bit → index entry. A
  crash at any point leaks at worst the MFT record allocation. We
  already mostly do this.
- **Intention log**: write a tiny "I'm about to X" record in a
  dedicated scratch attribute, replay on mount. Essentially
  mini-journal. ~500 LOC.

(Phase W5 / `$LogFile` writeback + replay covered above is the
full-journaling alternative; intentionally skipped per the original
W5 decision.)

### §6.4 Tracing hooks

A `tracing` subscriber or `log` call at attribute read / cluster
alloc / bitmap flip so consumers can instrument real-world usage.
Particularly useful for debugging FSKit reports.

### §6.5 `Send` safety contract for `CallbackReader` context pointer

**Today**: `unsafe impl Send for CallbackReader {}` at
`src/lib.rs:159-161` carries a generic two-line Safety comment
("context pointer is managed by the caller… FSKit guarantees serial
access").

**Problem**: FSKit serialises *callbacks* per volume — that
guarantees mutex-free aliasing, not thread-binding of the handle.
If the Swift caller's `context` refers to a thread-confined object
(e.g. an `@MainActor`-bound `FSBlockDeviceResource` that enforces
main-thread drop), transferring drop to another thread silently
violates that invariant. Papered-over unsafety; unlikely to bite
today but worth tightening before a confined-context consumer hits
it.

**Fix**: expand the Safety comment to spell out the contract the
Swift caller must uphold for their `context` pointer (Send-safe,
drop-on-any-thread). ~10 lines of Rust prose. No code change. The
"add an `fs_ntfs_drop_on_thread(handle)` helper" suggestion from
the original review is **deferred** — docs-only is the chosen fix.
Migrated from code-review-2026-04-19 §3.

### §6.6 Concurrency contract on `update_mft_record`

**Today**: `update_mft_record` (`src/mft_io.rs:271-289`) reads a
record, applies USA fixup, calls the mutator, re-applies fixup,
writes back. The docstring covers the IN_USE rejection invariant
but says nothing about concurrent writers.

**Problem**: an external concurrent writer (Windows / NTFS driver,
or a second fs-ntfs caller against the same image) can interleave
between read and write, causing torn updates. fs-ntfs doesn't spawn
threads internally, advisory locks can't prevent external
concurrency anyway, and Cargo test workers each get their own
`_xxx.img` copy — but the contract is undocumented.

**Fix**: add a module-level doc comment stating that concurrent
writers to the same image are UB; callers must mount the image
read-only or serialise externally. ~5 lines of Rust prose. No code
change. The "advisory file locking" alternative from the original
review is **rejected** — can't prevent external Windows concurrency
even if held. Migrated from code-review-2026-04-19 §7.

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
