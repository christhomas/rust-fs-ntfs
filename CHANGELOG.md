# Changelog

## [Unreleased]

### Changed

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

#### Diagnostic-helper read APIs (2026-05-23 / 2026-05-24)

A family of read-only inspection helpers for byte-diff investigations
and external tooling. All have C ABI wrappers. See
[`docs/FUTURE_FEATURES.md` §3.11](docs/FUTURE_FEATURES.md) for the
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
