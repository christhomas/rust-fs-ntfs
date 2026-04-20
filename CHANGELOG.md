# Changelog

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
