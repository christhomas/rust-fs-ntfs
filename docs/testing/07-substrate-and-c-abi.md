# 07 — Substrate & C ABI

> *Two layers bracket the NTFS logic: the block-device substrate **below** it
> (where every byte enters and leaves) and the C ABI **above** it (where every
> other language calls in). Both are tested independently, because a bug in
> either sinks the driver no matter how correct the NTFS code is.*

```
        ┌─────────────────────────────────────────────┐
        │   C / C++ / Go / Swift / …  consumers         │
        └───────────────────────┬───────────────────────┘
                                │  C ABI  (fs_ntfs_*)        ← tested by capi_*.rs
        ┌───────────────────────▼───────────────────────┐
        │           fs-ntfs  NTFS read/write logic        │   ← docs 02–06
        └───────────────────────┬───────────────────────┘
                                │  BlockDevice trait
        ┌───────────────────────▼───────────────────────┐
        │   am-fs-core substrate                          │   ← tested by
        │   FileDevice · CallbackDevice · CachingDevice    │     vendor/rust-fs-core
        │   · FFI slices · LRU cache                       │
        └─────────────────────────────────────────────────┘
```

---

## The substrate: `am-fs-core` (88 tests)

Every read and write the NTFS driver performs goes through the `am-fs-core`
block-device abstraction (vendored at `vendor/rust-fs-core`, shared with sister
filesystem crates). If this layer mis-reads a block or mis-caches a write, the
NTFS code above it is working from corrupted bytes. So it has its own 88-test
suite, independent of NTFS.

| Component | What it is | Key tests (`vendor/rust-fs-core/tests/`) |
|---|---|---|
| `BlockRead` / `BlockDevice` | the core read / read-write traits, plus `Arc`/`Box` forwarding | `block_forwarding.rs` |
| `FileDevice` | backed by `std::fs::File`; optional read-only | `cache.rs`, `file_device_edge_cases.rs` |
| `CallbackDevice` | backed by host-process callbacks (the FFI path) | `cache.rs`, `callback_device.rs` |
| `CachingDevice` | LRU read-cache decorator | `cache.rs`, `caching_lru.rs`, `caching_cross_block.rs` |
| FFI slices | C-ABI windowed views over a parent device | `ffi_slice.rs`, `slice_rw.rs` |
| Composition | stacks like `Caching(File(slice))` | `composition_stacks.rs` |
| Streams / errors | stream adapters, unified error type | `stream_integration.rs`, `error.rs` |

The tests that matter most for data safety:

- **Read-only is enforced, not advisory.** `file_device_ro_write_rejected` and
  `slice_ro_write_returns_read_only_even_with_writable_parent` prove a read-only
  device *refuses* writes — you cannot accidentally mutate a volume you opened to
  inspect.
- **The cache cannot serve stale data.** `caching_device_invalidates_on_write`
  proves a write clears the cached copy of the affected block; `caching_lru.rs`
  proves eviction order is genuine LRU. A cache that returned a pre-write block
  would be silent corruption from the driver's point of view.
- **Bounds are real.** `read_past_eof_returns_short_read`,
  `slice_rw_write_past_length_returns_out_of_bounds`, and
  `open_nonexistent_returns_io_error` prove out-of-range access becomes a clean
  error, never a wild access — the foundation of the
  [no-panic guarantee](05-robustness-and-fuzzing.md).
- **Slices rebase correctly.** `slice_rw_write_lands_at_parent_start_plus_offset`
  proves a windowed view writes to the right absolute offset — the mechanism that
  lets the driver operate on a partition inside a larger image without trampling
  neighbors.

---

## The C ABI: `fs_ntfs_*` (16 test files, ~85 tests)

The crate ships a stable C ABI (declared in `include/fs_ntfs.h`) so it can be
linked from C, C++, Go (cgo), Swift, or anything with FFI. The FFI boundary has
its own failure modes that have nothing to do with NTFS — string marshalling,
out-pointer semantics, error-code mapping, handle lifetimes — so it has a
dedicated `capi_*` test suite that drives the C entry points exactly as a foreign
caller would.

| Surface | C entry points exercised | Test file |
|---|---|---|
| Create | `fs_ntfs_create_file` — return codes, path marshalling | `capi_create.rs` |
| Write content | `fs_ntfs_write_file` — overwrite / append / truncate modes | `capi_write_content.rs`, `capi_write_variants.rs`, `capi_write_w1.rs` |
| Truncate | `fs_ntfs_truncate` — sizing | `capi_write_truncate.rs` |
| Handle I/O | file-handle read/write | `capi_handle_rw.rs` |
| Remove | `fs_ntfs_unlink` / `fs_ntfs_rmdir` | `capi_remove.rs` |
| Link | `fs_ntfs_create_link` (hard links) | `capi_link.rs` |
| Rename | `fs_ntfs_rename` | `capi_rename.rs` |
| ADS | alternate-data-stream API | `capi_ads.rs` |
| EA | extended-attribute read/write | `capi_ea.rs` |
| Reparse | symlink / reparse API | `capi_reparse.rs` |
| Block I/O | low-level `fs_core` device over the ABI | `capi_fs_core_rw.rs` |
| fsck | `fs_ntfs_fsck`, dirty-clear, `$LogFile` reset | `capi_fsck.rs`, `capi_fsck_callbacks.rs`, `capi_fsck_fs_core_device.rs` |

What these specifically guard:

- **Two transports, both tested.** Every operation is reachable both by file path
  and through a *callback transport* (the embedder supplies read/write callbacks
  — used by GUI apps and OS-integration shims that don't hand over a file path).
  `capi_fsck_callbacks.rs` and `capi_fsck_fs_core_device.rs` prove the callback
  path behaves identically to the path-based one.
- **Error codes map cleanly.** `errno_companion.rs` (5 tests) verifies the
  driver's errors translate to stable, POSIX-style error codes a C caller can
  branch on — not opaque panics across the FFI boundary.
- **String safety.** Paths go through a single `cstr_to_path` helper at 61 call
  sites; byte-string arguments (EA / stream names, which are *not* paths) use raw
  `CStr` deliberately. The tests exercise both so a non-UTF-8 or non-terminated
  argument is handled, not assumed away.

---

## Why both ends matter for your data

The NTFS logic in [02–06](02-read-path.md) can be flawless and you can still lose
data if:

- the cache hands the driver a stale block (caught by the substrate tests), or
- a Go program calls `fs_ntfs_write_file` and the length argument is mis-marshalled
  across the ABI (caught by the `capi_*` tests).

By testing the substrate and the ABI as first-class layers — 88 + ~85 tests —
the safety argument holds end to end, from the foreign-language call all the way
down to the byte hitting the disk.

---

**Next:** [08 — Coverage map & honest limits →](08-coverage-and-honest-limits.md)
