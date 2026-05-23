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

### W2.5 — long-filename / attribute-list edge case (flag only)

Resident-only attributes that could outgrow an MFT record:

- `$FILE_NAME` — up to 255 UTF-16 code units (≈ 510 bytes payload +
  66 byte $FILE_NAME header + 24 byte attribute header). With
  multiple hard links + a large `$SECURITY_DESCRIPTOR`, a 1024-byte
  record could in principle exhaust. `$FILE_NAME` is required to
  be resident per MS-FSCC, so the answer when this happens is an
  `$ATTRIBUTE_LIST` (extension record), not promotion.
- `$ATTRIBUTE_LIST` itself can be non-resident if it grows.

Today neither path is implemented. The realistic exposure is small
(record sizes are 4096 bytes by default, capacity ~3700 bytes after
fixed overhead). Suggested next step is a negative test that creates
a file with a 255-character UTF-16 name + several hard links and
verifies we either fit (likely) or fail loudly (not silently
corrupt).

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

**Status (2026-05-23)**: still open. `chkdsk DRIVE:` (readonly) now
exits 0 on every passing matrix scenario after the
indexed_flag / bytes_used / INDEX_HEADER.alloc / namespace fixes
(see [`docs/mkfs-bug-catalog.md`](mkfs-bug-catalog.md) — Iter N).
`chkdsk DRIVE: /scan` still exits 13. The matrix's `clean`
verdict shape accepts `readonly == 0 AND scan ∈ {0, 11, 13}`,
so the matrix being green does not mean /scan is clean. Tighten
to `scan == 0` once the differentiator is found.

**What's known**:

- All currently-passing matrix scenarios (35/42 confirmed at the
  time of writing, full run in progress) reach `readonly = 0`.
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
- The Iter N fixes (FILE_NAME indexed_flag = 1; MFT bytes_used to
  include the 8-byte END trailer; INDEX_HEADER alloc_size kept in
  sync with total_size; FILE_NAME.namespace derived from DOS-8.3
  fit instead of hardcoded WIN32_AND_DOS, both MFT-side and index-
  entry-side) lifted `readonly` from "errors found" to 0 across the
  matrix, but did **not** shift /scan. So the differentiator is in
  whatever /scan validates that readonly skips.

**Productive next moves** (not yet attempted):

1. Capture every disk read `chkdsk /scan` performs against our volume
   via Windows Procmon on the test VM, correlate with what /scan does
   against the reference. The reads /scan does that readonly doesn't
   pinpoint exactly which bytes the validator keys on. Harness already
   exists at `scripts/procmon-chkdsk-trace.ps1`.
2. Implement S1–S5 (see `docs/implementation-plan-secure-and-extend.md`):
   ship populated `$Secure:$SDH`/`$SII` view indexes + `$Extend`
   directory with `$Reparse` and `$RmMetadata` sub-files. Iter H's
   Procmon trace identified these three structures as files /scan
   opens that we don't ship.
3. Time-bisect: Mount-DiskImage with `-NoDriveLetter`, manually run
   `Set-Disk -IsOffline $false`, then assign letter — different
   sequencing might shift ntfs.sys's first-mount-state behaviour.

**Effort estimate**: medium-to-large (S1–S5 is the most-promising
path; multi-day. The bisection / Procmon work is hours).

### §3.2 NTFS compression (LZNT1)

- **Read**: upstream `ntfs` 0.4 does not expose LZNT1 decompression.
  `fs_ntfs_read_file` on a compressed file today returns whatever
  `NtfsAttributeValue::read` produces, which is the raw (still-
  compressed) bytes — silent garbage, not a clear error. Cheap
  improvement worth doing first: detect `$DATA.flags & 0xFF != 0`
  (`compression_unit` non-zero) in `fs_ntfs_read_file` and return
  a clear error ("file is compressed; LZNT1 decompression not yet
  supported") so callers know.
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
empty unnamed `$DATA` + `IO_REPARSE_TAG_WOF` (0x80000017) +
`WofCompressedData` ADS. Without WOF support, reading `notepad.exe`
returns 0 bytes. **Silent data loss on every modern volume.**

Today we recognise the tag in the reparse-dispatch switch
(`src/lib.rs:575–609`) but fall through to "leave file_type alone"
and read the empty unnamed `$DATA` as-is, hence the 0 bytes. Cheap
improvement: in `fs_ntfs_read_file`, detect tag 0x80000017 on a
file and return a clear error ("WOF-compressed; not yet supported")
so callers know.

Real fix requires XPRESS4K/8K/16K + LZX decompression of the
`WofCompressedData` ADS. Third-party crate (`ms-compress`) does it;
bindings would be ~200 LOC. Listed as the biggest single
read-correctness gap in STATUS.md cross-check "#### WOF (Windows
Overlay Filter) compression not supported".

### §3.9 Case-sensitive directory flag

`FILE_CASE_SENSITIVE_DIR` (WSL / Docker-Desktop). Our writes never
set it; our reads never check it. On a dev volume with
case-sensitive subdirs, we collapse `foo.txt` and `FOO.TXT` to
whichever the B-tree finds first. (STATUS.md cross-check
"#### Per-directory case-sensitivity flag ignored".)

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

### §5.9 Test-matrix Stage A — 2 GiB raw-write cap

**Status**: known gap; `scripts/run-scenario.ps1` Stage A throws on
images >2 GiB ("PS ReadAllBytes / chunked-WriteFile both fail past
2 GiB; this scenario needs further work"). See the inline TODO in
the same file.

**What's known**:

- `[System.IO.File]::ReadAllBytes` is hard-capped at 2 GiB by
  PowerShell's CLR — no go for the 4 GiB / 16 GiB Tier-1 scenarios.
- A chunked `FileStream` write to `\\.\PhysicalDrive$N` fails with
  "Access to the path is denied." — likely alignment /
  `FILE_FLAG_NO_BUFFERING` requirements that the chunked path
  doesn't satisfy. The smaller-volume bulk path works because
  Windows accepts the single `WriteFile` call as a whole-buffer
  transfer.

**Productive next moves**:

1. Open the raw device with `CreateFile` + `FILE_FLAG_NO_BUFFERING`
   and align both buffer + offset to the device's logical sector
   size. Try a 16 MiB sector-aligned chunk loop.
2. Skip the wrapper VHDX entirely for >2 GiB volumes — `qemu-img
   convert -O vhdx -o subformat=fixed,partitioning=gpt` may produce
   a partitioned VHDX directly without the raw-write step. Worth
   a prototype.
3. Use a temporary loop-mounted RAM disk on the VM to stage the
   image, then `Move-Item` to the partition. Avoids the
   PhysicalDrive write entirely.

**Effort estimate**: small (~2-4 h) once one of the above is
chosen and prototyped on the VM. Blocks Tier-1 scenarios
`mac-format-volume-{4gib-cluster-4k,4gib-cluster-64k,16gib-cluster-4k}`.

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
