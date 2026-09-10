# Changelog

## [Unreleased]

### Fixed

- The index header is read inside the attribute value that holds it.
  `find_index_entry` and `collect_entries` read `first_entry_offset`
  and `total_size` bounded by the whole MFT record, so a resident
  `$INDEX_ROOT` value shorter than 32 bytes took them from the next
  attribute; the entry end then clamped below the entry start and the
  walk answered `Ok(None)` / no entries. A directory with entries
  listed as empty, and the four collision checks in `write.rs` read
  that as "the name is free". `index_root_flags` had the same bound
  and returned a flags byte belonging to another attribute; it now
  answers `None`, and `read_dir_entries` treats that as an error
  rather than as "no subnodes". `collect_index_root_entries` also names
  a non-resident `$INDEX_ROOT` as the reason it refuses one, instead of
  reporting `no value_offset` from a field the iterator happens to
  leave unset — the other two `$INDEX_ROOT` readers already did.
- A read offset is checked against the volume before anything is read.
  The three per-cluster sites in `read.rs` multiplied a disk-supplied
  LCN by the cluster size raw; a run past the end of the filesystem but
  inside the device returned its bytes as the file's contents, and an
  LCN near 2^52 wrapped the product to a low offset in a release build.
- `$ATTRIBUTE_LIST` is consulted before the base record, not after. A
  run list split across records has its VCN-0 segment in the base, so
  the guard that promised to refuse a split attribute sat behind the
  only path that reached it and a fragmented file read back its first
  segment with `Ok`.
- A write past `initialized_length` moves it, so the bytes are
  readable. Growing a file and writing into the region grown reported
  success and then read back as zeros, with the data on the platter and
  unreachable. A write that starts past the field zero-fills the gap
  first rather than publishing whatever the clusters held before.
- The index lookups collate the way the index is ordered. Entries go
  into `$I30` under `COLLATION_FILE_NAME` and were looked up by exact
  UTF-16 equality, so `Foo.txt` passed the collision check beside
  `foo.txt` and a duplicate collation key was written; `unlink` could
  not find an entry path resolution had just found. A case-only rename
  stays allowed.
- The write guards catch the flags they say they catch. `write_at`,
  `truncate` and `grow` masked `0x00FF` — the compression-unit field —
  while claiming to refuse sparse (`0x8000`) and encrypted (`0x4000`)
  attributes, so both were written to. One shared predicate now, with
  the mask beside the offset it is read from.
- `sectors_per_cluster` has a second encoding above `0x80`, and volumes
  use it. Read as a literal, a 128 KiB-cluster volume decoded to a
  126976-byte "cluster" and every offset the driver computed was wrong
  by that factor without anything failing.
- `$MFTMirr` declares the four records it holds rather than the cluster
  it fills. At 32 KiB clusters and above the mirror said it held eight
  or sixteen records and held four followed by zeros, which a recovery
  would have written over live system records.
- An allocation the volume cannot honour fails before it is written.
  `$Bitmap`'s declared bit count is now bounded by the volume's cluster
  count, and the two promotion paths and the sparse writer route their
  offsets through the checked helper the rest of the write path uses.
- The index ends where the attribute holding it ends, on the read side
  too. An `$INDEX_ROOT` whose `total_size` exceeded its own value had
  `readdir` decoding the bytes after the attribute as entries, and
  `rmdir` deleted a directory whose index header it could not read.
- A WOF-compressed file is refused by both front doors. The C ABI
  detected `IO_REPARSE_TAG_WOF` and failed loudly; the Rust API did not,
  and returned the right number of zero bytes for every file `compact
  /exe` or Compact OS had touched.
- An index block that leaves its run is refused rather than transferred
  anyway. On a volume with clusters smaller than the 4096-byte index
  block, a block landing across a run boundary took its tail from — and
  wrote its tail over — the next file's clusters.

### Changed

- **BREAKING.** `index_io::IndexEntryLocation` gains a required public
  field, `sequence: u16`, and is now `#[non_exhaustive]`. Code that
  built one with a struct literal no longer compiles; code that reads
  the fields is unaffected, which is every use inside this crate and
  the only use the type was designed for -- it is returned by
  `find_index_entry` and `find_entry_in_indx_block`, not constructed by
  callers.

  There is no version of this that is not a break. The field carries
  the high 16 bits of the `$INDEX_ROOT` entry's file reference, which
  the decode sites used to mask off and throw away; a reference's
  sequence number is what distinguishes a live entry from one left
  behind by an interrupted delete, and a value that is discarded before
  anyone sees it cannot be checked by anything. Adding
  `#[non_exhaustive]` on its own would have broken the same literals,
  so it arrives in the same release rather than costing a second one.

  Callers that constructed the type -- most likely in a test -- should
  take one from `find_index_entry` instead.

- The shared `am-fs-core` sibling checkout moves to v0.2.10, in
  `Cargo.toml`, `chores.yml` and both workflows' Windows clone.

## [0.4.0] — 2026-09-06

### Fixed

- An index edit stays inside the index it is editing. For an
  $INDEX_ROOT the buffer is the whole MFT record and the index lives in
  one attribute's resident value; the shift and the zero-fill were
  bounded by the record, so they ranged over every other attribute and
  the shredded record was written back reporting success.
- Removing or inserting an index entry consults IE_FLAG_HAS_SUBNODE. An
  entry may carry a child VCN, and shifting it away orphaned the whole
  subtree -- invisible to this crate's own reader, which scans rather
  than descends, and visible to chkdsk.
- A file's runs may not free the volume's own clusters. unlink and
  truncate pushed every run into the bitmap bounded only by $Bitmap's
  size, so a record whose runs overlap $MFT freed live system clusters.
- A write offset is checked against the volume before anything is
  written.
- The driver is bounded against the bytes it is handed at the trust
  boundary: MFT record validation, on-disk lengths used as allocation
  sizes, an unclamped $LogFile fill, index-entry name walks, and two
  bitmap walks that could fail to terminate.

## [0.3.5] — 2026-09-04

### Changed

- **This crate no longer uses git submodules.** `am-fs-core` resolves
  from the sibling checkout at `../rust-fs-core` and the Windows test
  harness from `../fs-test-harness`, pinned by `chore siblings` — which
  is how the other thirteen crates in the family have always worked.
  This repo was the last one out of step.

  A submodule pins a copy per consumer, so drift is silent: measured
  across the workspace, `rust-fs-core` existed as five checkouts at four
  different versions and nothing reported it. A shared sibling has one
  copy, which makes that class of drift unrepresentable rather than
  merely detectable.

- **The lockfile moves `am-fs-core` 0.2.2 → 0.2.4.** It had been pinning
  a version two releases behind the rest of the family — exactly the
  drift the submodule made invisible.

- `chore build` and `chore test` now verify the siblings are at their
  pinned tags before compiling, and fail with both versions named if
  they are not.

### Fixes

- **The create rollback is covered.** `undo_new_record_io` was used at
  six sites but had no test: disabling either half — clearing `IN_USE`,
  or freeing the `$MFT:$Bitmap` bit — left the whole suite green. Four
  tests now cover it. Without the rollback a failed create leaves an MFT
  record allocated and marked in use that no directory names, which is
  the orphan record chkdsk reports.

- `clear_in_use: bool` became `NewRecordState::{BitmapOnly, RecordWritten}`.
  The distinction is not obvious from a boolean: between taking the
  bitmap bit and writing the record there is a window where the record
  on disk is still whatever was there before, and clearing `IN_USE` in
  it would be editing a record this create never wrote.

## [0.3.4] — 2026-08-29

### Fixed

- **A resident `$MFT:$Bitmap` now reads back what it just wrote, so the
  MFT allocator's rollback works.** The resident layout served reads
  from a copy of the bitmap taken when the attribute was located, while
  writes patched `$MFT`'s record on disk — so a bit set by `allocate_io`
  was invisible to the next read. `create_file`/`mkdir` set the bit
  before writing the new record and free it again if a later step fails;
  that free re-read the stale copy, reported `MFT record N already free`
  and cleared nothing, leaking the record. The resident value is no
  longer cached at all: both layouts read through the same path their
  writes take. The create/mkdir rollbacks also stop discarding their own
  errors — the original failure is still what the caller receives, with
  a failed rollback appended to it rather than swallowed.

## [0.3.0] — 2026-06-02

### Fixed

- **Delete now frees the clusters of *every* non-resident attribute**,
  not just the unnamed `$DATA`. A file owning named non-resident
  `$DATA` streams — or any attribute promoted to non-resident —
  previously leaked their runs in `$Bitmap` on last-link `unlink` and
  on rename-replace over such a file. `remove_file_record_io` now walks
  the base record and reclaims all of them. (Attributes living in
  `$ATTRIBUTE_LIST` extension records remain a tracked follow-up.)

### Changed

- **`ntfs` crate demoted to a `dev-dependency`.** The native read layer
  now backs every production path (`resolve_path`, attribute reads,
  `read_stat`, directory enumeration, volume info, the fsck
  `$Volume`/`$LogFile` locators); the upstream crate survives **only**
  as the test oracle those decoders are cross-checked against.
  `cargo tree -e no-dev -i ntfs` is empty — no production code links it.

- **Test-matrix disk hygiene (tooling).** The harness runner
  (fs-test-harness ≥ v3.11.0) stamps an `owner.pid` into each staged
  image dir and reaps dirs whose owner pid is no longer alive, and
  `scripts/run-matrix.sh` self-heals a stale per-filter lock left by a
  killed run. Cancelled matrix runs no longer pile up tens of GiB of
  staged images.

- `$VOLUME_INFORMATION` upgrade-on-mount now fires across **every**
  RW entry point, not just `fs_ntfs_mount_rw_with_fs_core_device`.
  Newly wired:
  - `fs_ntfs_mount` (path-based; the path-mount is RW-capable since
    mutators re-open RW per call).
  - `fs_ntfs_mount_with_callbacks` when the caller supplied a `write`
    callback (skipped otherwise — the mount is effectively RO).
  - `Filesystem::mount_rw()` — new facade entry point that wraps
    `mount` + `upgrade_volume_version`. The `rust-ntfs` CLI's
    mutating commands (`touch`, `mkdir`, `rm`, `rmdir`, `write`)
    switched to this; `ls` stays on `mount` since it's read-only.

  All upgrade attempts remain best-effort: failure is logged at
  `warn` and never fails the mount.

### Added

- **Atomic rename-with-replace.** `fs_ntfs_rename2_h(fs, old_path,
  new_basename, flags)` plus the `FS_NTFS_RENAME_REPLACE` (`0x01`) flag
  atomically overwrite an existing destination (POSIX `rename(2)`
  semantics): file→file frees the old record + clusters,
  empty-dir→empty-dir overwrites, and crossing the file/directory
  boundary or targeting a non-empty directory fails with
  EISDIR / ENOTDIR / ENOTEMPTY. Unknown flag bits are rejected with
  EINVAL; plain `fs_ntfs_rename_h` keeps reject-existing semantics.
  Core: `write::rename_replace_io`. Lets in-place editors (write-temp,
  rename-over-original) succeed without a non-atomic unlink-first.

#### Diagnostic-helper read APIs (2026-05-23 / 2026-05-24)

A family of read-only inspection helpers for byte-diff investigations
and external tooling. All have C ABI wrappers. See
[`docs/future-features.md` §3.11](docs/future-features.md) for the
full per-API table.

- `read_attributes` / `describe_attributes` — every attribute on a
  file's MFT record (type code + name + dimensions).
- `read_file_names` — every `$FILE_NAME` on a file (multi-namespace
  files surface as multiple records).
- `read_security_id` — `$STANDARD_INFORMATION.security_id` (`None`
  for the 48-byte v1.x form, `Some(id)` for 72-byte v3.x).
- `read_object_id_extended` — full 64-byte `$OBJECT_ID` (object_id
  + Birth GUIDs) when present.
- `read_volume_label` — `$VOLUME_NAME` decoded to UTF-8.
- `fs_ntfs_get_volume_info_v2` — v2 extension carrying
  `volume_flags` / `is_dirty` / `mft_record_size` / `bytes_per_sector`
  on top of v1.
- `read_reparse_point` — raw `(reparse_tag, data)` payload for any
  reparse type, complement to `fs_ntfs_readlink` (symlink-only).
- `list_named_streams` — names of every named `$DATA` attribute
  (ADS), excluding the unnamed primary.
- `list_ea_keys` — EA names only (cheap enumeration; skips values
  up to 64KB each).
- `read_si_full` — every MS-FSCC §2.4.2 `$STANDARD_INFORMATION`
  field including the optional NTFS 3.x trailer (owner_id,
  security_id, quota, usn).

#### Writer-side additions (2026-05-23 / 2026-05-24)

- `set_security_id` — point a file at an existing `$Secure:$SDS`
  entry. Adding new SD entries is separate, larger work.
- `write_object_id` — runtime 16-byte `$OBJECT_ID` writer.
- `write_object_id_extended` — 64-byte form carrying the mandatory
  object_id plus the three DLT Birth GUIDs (MS-FSCC §2.4.6).
  `fs_ntfs_set_object_id_extended_h` is the handle-based sibling
  for callers holding an open filesystem handle.
- `set_volume_label` / `read_volume_label` — rename / clear the
  volume `$VOLUME_NAME`. Empty label removes the attribute.
- `compare_names_ordinal` — case-sensitive collation primitive
  (foundation for `CASE_SENSITIVE_DIR` work, kept off-by-default).

#### Volume-version handling (2026-05-21)

- `fsck::upgrade_volume_version` (path + `FsckIo` variants) and
  `Filesystem::upgrade_volume_version()` — mimic `ntfs.sys`'s
  "upgrade on mount" transition: rewrite `$VOLUME_INFORMATION` from
  `major=1, minor=2 + UPGRADE_ON_MOUNT` (the fresh-format state
  Microsoft `format.com` and our `mkfs` produce) to `major=3,
  minor=1` with the flag cleared. Idempotent; returns `Ok(true)` on
  upgrade, `Ok(false)` if the volume didn't match the pattern.
- `fs_ntfs_mount_rw_with_fs_core_device` now invokes the upgrade
  best-effort on every RW mount, so volumes touched by our driver
  look "already upgraded" when they later reach Windows — parallel
  to what `ntfs.sys` would do on first RW mount. Upgrade errors are
  logged at `warn` and don't fail the mount.

#### Test infrastructure

- Seal-by-binary-hash matrix discipline:
  `scripts/matrix-baseline.sh` (run full 42-scenario matrix and
  write `test-diagnostics/matrix-results.json`) +
  `scripts/matrix-verify.sh` (quickly check whether the working
  tree's binary is sealed by the committed JSON). Uses
  `--remap-path-prefix` so `binary_sha256` is path-stable across
  worktrees / machines and survives rebase / squash-merge.
- 42/42 sealed matrix runs recorded for both PR #49 staging tip
  (`30fcdd6`, 11369s) and staging-2 tip (`d9595c7`, 13794s).

### Build / packaging

- `am-fs-core` is now vendored as a git submodule at
  `vendor/rust-fs-core` instead of an unmanaged `../rust-fs-core`
  sibling path. A fresh `git clone --recurse-submodules` (or
  `git submodule update --init --recursive` in an existing checkout)
  is now sufficient to build — no manual side-by-side checkout
  required. Cargo.toml's path dep now points at `vendor/rust-fs-core`.

### Fixed

#### PR #49 review feedback (2026-05-24)

CodeRabbit + greptile flagged 24 inline issues on PR #49; 23 fixed
in commit `d28e200` ("fix(pr49): address CodeRabbit + greptile review
feedback"), 1 (record_build preflight) deferred to its own focused PR.

- `fn_namespace_for` (`src/record_build.rs`):
  - Reject empty stems (`.foo`, `.env`, `.rc`) — chkdsk rejects
    WIN32_AND_DOS on an empty 8.3 stem.
  - Reject trailing-dot names (`foo.`) — same rule.
  - Validate every character against the canonical DOS 8.3 alphabet
    (ASCII alphanumerics plus `$ % ' - _ @ ~ \` ! ( ) { } ^ # &`).
    Names with spaces, Unicode, control chars, or reserved
    punctuation now classify as POSIX (Bug 12/13 regression risk
    closed).
- `read_volume_label` (`src/write.rs`): odd `val_len` now returns
  `Err("$VOLUME_NAME has odd byte length: N (must be multiple of 2
  for UTF-16)")` instead of `Ok(String::new())`. Corruption is no
  longer indistinguishable from a missing label.
- `read_object_id_extended_io` (`src/write.rs`): rejects any
  `val_len` other than 16 or 64 with a descriptive error. Was
  silently accepting `val_len >= 16` and dropping extras.
- `remove_attribute_at` helper (`src/write.rs`): extracted from
  five identical `copy_within(loc.attr_offset + old_len..bytes_used,
  ...)` call sites and added explicit bounds validation
  (`attr_length > 0`, `bytes_used <= record.len()`,
  `attr_offset + attr_length <= bytes_used`) before touching memory.
  Malformed on-disk records now return `Err` instead of panicking
  inside `copy_within`.
- `fs_ntfs_get_volume_info_v2` (`src/lib.rs`): zero-init
  `ntfs_version_major` / `ntfs_version_minor` before the
  `if let Ok(vol_info) = ...` block, so the fields are defined on
  early-return.
- `FsNtfsVolumeInfoV2._pad` (`src/lib.rs` + `include/fs_ntfs.h`):
  bumped from `[u8; 3]` to `[u8; 5]` so the full gap between
  `is_dirty` (offset 170) and `mft_record_size` (offset 176, u32
  alignment) is explicit. No hidden compiler padding; ABI unchanged
  (the layout was already what the compiler emitted — we just made
  it visible in source).
- `fs_ntfs_get_volume_info_v2` doc (`include/fs_ntfs.h`): removed
  the "at their own risk" language for v1-sized buffers. Callers
  now MUST allocate a `fs_ntfs_volume_info_v2_t`-sized buffer or
  larger.

#### Test-infra / scripts (2026-05-24)

- `scripts/matrix-baseline.sh`:
  - Reject unknown CLI flags with `exit 2` (was silently falling
    through to full-matrix mode).
  - Smoke loop no longer swallows scenario failures via `|| true`;
    now tracks `smoke_failed=1` per failing scenario, continues so
    metadata still gets collected, then `exit 1` after the JSON
    write if any scenario errored. Gate contract is enforced again.
- `scripts/matrix-verify.sh`: SHA fast-path now also requires
  `git diff-index --quiet HEAD --`. A dirty tree falls through to
  the binary-hash check instead of false-positive-sealing.
- `scripts/v2/_lib.ps1` (Sync-VhdToImg): `if ($n -le 0) { break }`
  was silently producing partial `.img` files on short raw-device
  reads. Now `throw` after the loop if `$remaining > 0`.
- `scripts/win/verdict-collect.ps1`: hardcoded
  `C:/Users/chris/dev/rust-fs-ntfs-matrix/diag/v2` replaced with a
  `-Root` parameter defaulting to `$env:USERPROFILE/...` (built
  with `Join-Path`). Was silently producing empty output on any
  other VM/user.
- `scripts/_matrix-build-json.py`: `vm.address` field redacted
  to `"<redacted>"` instead of embedding `$VM_HOST` verbatim. Prior
  committed JSON entry (`chris@192.168.213.147` in
  `test-diagnostics/matrix-results.json`) was also redacted.

#### Hygiene

- `cargo fmt --check` across the workspace (was blocking CI on
  several files: `src/lib.rs`, `src/record_build.rs`, `src/write.rs`,
  `tests/object_id.rs`, `tests/read_file_names.rs`,
  `tests/security_id.rs`, `tests/volume_info_v2.rs`,
  `tests/volume_label.rs`).
- Pre-commit hook (`.githooks/pre-commit`) was already shipped but
  not auto-installed; `core.hooksPath` set to `.githooks` so future
  commits run `cargo fmt --check` + `cargo clippy -- -D warnings`
  locally before push. One-shot install:
  `bash scripts/install-hooks.sh`.

#### Volume-version reader (2026-05-21)

- `Filesystem::volume_info()` now reads `$VOLUME_INFORMATION` off
  disk via upstream `ntfs.volume_info()` instead of returning a
  hardcoded `(major: 3, minor: 1)`. A fresh-format volume produced
  by `mkfs` correctly reads back as 1.2 (matches Microsoft
  `format.com`; `ntfs.sys` upgrades to 3.1 on first RW mount). The
  C ABI path (`fs_ntfs_get_volume_info`) was already reading the
  real bytes; only the Rust facade was lying.

## [0.1.2] — 2026-04-20

### Docs / packaging

- README fully rewritten. New sections: origins, architecture diagram,
  a concrete capability matrix contrasting fs-ntfs with upstream
  `ntfs = "0.4"` (justifying this crate's existence as a read/write
  driver with fsck + stable C ABI), explicit scope / supported vs.
  not-implemented list, and a plain-English at-your-own-risk
  disclaimer restating the MIT/Apache-2.0 no-warranty clauses.
- Framing neutralised: crate is described as a general-purpose FFI
  NTFS driver. DiskJockey is mentioned once as a production user
  with an explicit no-coupling note; no more `Swift` / `FSKit`-
  specific language in the API description.
- `Cargo.toml` description updated to match (`FFI from C/C++/Go/etc.`
  instead of `Swift/C/Go/etc.`) and `version` bumped to `0.1.2` to
  match the new tag (previous releases were tag-only; the manifest
  still read `0.1.0`).
- No code or ABI changes. `libfs_ntfs.a` behavior is unchanged vs.
  0.1.1.

## [0.1.1] — 2026-04-20

### Added — callback-based fsck

New C ABI so FSKit (and other FFI consumers holding a block device
via callbacks rather than a filesystem path) can check the dirty
flag + repair without opening `/dev/diskN` themselves:

- `fs_ntfs_blockdev_cfg_t` gains an optional `write` callback.
- `fs_ntfs_is_dirty_with_callbacks(cfg)` — callback-based dirty check.
- `fs_ntfs_fsck_with_callbacks(cfg, progress_cb, progress_ctx,
  out_logfile_bytes, out_dirty_cleared)` — callback-based repair
  with optional progress emission. Progress callback signature:
  `(context, phase, done, total)` where phases are `"reset_logfile"`
  (per 64 KiB chunk) and `"clear_dirty"` (once at start/end).

Path-based API (`fs_ntfs_fsck`, `fs_ntfs_is_dirty`,
`fs_ntfs_clear_dirty`, `fs_ntfs_reset_logfile`) unchanged. Internal
refactor around an `FsckIo` trait — `PathIo` wraps `std::fs::File`;
`CallbackIo` wraps raw function pointers + context.

## [0.1.0] — unreleased

First public release.

### C ABI — `fs_ntfs_*`

C-ABI wrapper around [ColinFinck/ntfs](https://github.com/ColinFinck/ntfs)
so non-Rust callers can mount and read NTFS volumes.

Surface (see `include/fs_ntfs.h` for full signatures):

- Lifecycle: `fs_ntfs_mount`, `fs_ntfs_mount_with_callbacks`,
  `fs_ntfs_umount`, `fs_ntfs_get_volume_info`.
- Metadata: `fs_ntfs_stat`, `fs_ntfs_last_error`.
- Directories: `fs_ntfs_dir_open`, `fs_ntfs_dir_next`, `fs_ntfs_dir_close`.
- Files: `fs_ntfs_read_file`.

### Scope

Read-only. Writes are not implemented (and the upstream `ntfs` crate
does not provide write support at this time).

### Origin

Extracted from the `ntfsbridge/` crate in
`github.com/christhomas/ext4-fskit` (now archived). Renamed symbols
`ntfs_bridge_*` → `fs_ntfs_*`, lib `libntfsbridge.a` → `libfs_ntfs.a`,
header `ntfs_bridge.h` → `fs_ntfs.h`. Cargo dep on the upstream `ntfs`
crate switched from a path-vendored submodule to the crates.io release.
