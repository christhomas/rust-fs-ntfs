# Code Review: fs-ntfs

**Date:** 2026-04-19  
**Reviewer:** Cascade AI  
**Commit Range:** Current working tree

---

## Summary

This review identifies critical correctness bugs (timestamp truncation, name buffer overflow), thread-safety concerns, and architectural gaps in the NTFS driver implementation. The STATUS.md accurately documents many of these issues—this review validates those findings and adds additional edge cases.

---

## Critical Issues (Must Fix)

### 1. Timestamp Truncation (Y2038 + Precision Loss)

**Location:** `src/lib.rs:288-294`

```rust
fn ntfs_time_to_unix(ntfs_time: ntfs::NtfsTime) -> u32 {
    const EPOCH_DIFF: u64 = 11_644_473_600;
    let secs = ntfs_time.nt_timestamp() / 10_000_000;
    secs.saturating_sub(EPOCH_DIFF) as u32
}
```

**Problems:**
- `as u32` truncates values ≥ 4,294,967,296 seconds—dates after 2106-02-07 wrap to small values instead of saturating
- Sub-second precision (100ns resolution) is discarded unconditionally
- Pre-1970 dates clamp to 0 (acceptable), but 2106+ dates silently corrupt

**Impact:** Silent mtime corruption on archive volumes, backup-tool sentinel dates (e.g., 2099), and mismatch against SMB/Win32 peers that compare at 100 ns resolution.

**Fix:** Widen ABI to 64-bit seconds + nanoseconds as suggested in STATUS.md §1.1.

---

### 2. Dirent Name Buffer Truncates Legal NTFS Names

**Location:** `include/fs_ntfs.h:53`, `src/lib.rs:253`

```rust
name: [u8; 256],  // 256 bytes
```

NTFS allows 255 UTF-16 code units → up to 1020 UTF-8 bytes + NUL. A CJK filename with 86+ characters exceeds this buffer. The copy at `src/lib.rs:730-735` silently truncates.

**Impact:** A file with non-BMP names (emoji, rare CJK) or long CJK names comes back with a corrupted, non-roundtrippable name. A subsequent `fs_ntfs_stat` on that truncated name fails with ENOENT. This is a silent data-loss path hit by any FSKit enumeration of user profile / OneDrive dirs.

**Fix:** Widen to `name[1024]` (NTFS-3G uses this), or return variable-length with a caller-owned buffer.

---

### 3. `unsafe impl Send for CallbackReader` Claims Too Much

**Location:** `src/lib.rs:156`

```rust
unsafe impl Send for CallbackReader {}
```

This assumes the caller's `context` pointer is safe to move across threads, but FSKit only guarantees callback serialization per volume—not thread-binding. If the Swift context is thread-confined (e.g., `@MainActor`), dropping on another thread violates that contract.

**Impact:** If the Swift caller's `context` refers to a thread-confined object, transferring drop to another thread violates that invariant. Papered-over unsafety.

**Fix:** Document the `Send`-safety contract the Swift caller must uphold for their `context` pointer; consider a `fs_ntfs_drop_on_thread(handle)` helper for confined contexts.

---

## Medium-Severity Issues

### 4. Integer Overflow Risk in `grow_nonresident`

**Location:** `src/write.rs:492-494`

```rust
let new_last_vcn = (new_size - 1) / cluster_size;
let new_allocated = (new_last_vcn + 1) * cluster_size;
```

If `new_size` is 0, this underflows (`new_size - 1`). The function checks `new_size <= current_len` earlier but should still use `checked_sub`/`checked_add` for defense-in-depth.

**Fix:** Use `checked_sub` and `saturating_add` or add explicit bounds checks.

---

### 5. Silent Error Swallowing in Directory Iteration

**Location:** `src/lib.rs:709-717`

```rust
while let Some(entry_result) = iter.next(&mut bridge.reader) {
    let entry = match entry_result {
        Ok(e) => e,
        Err(_) => continue,  // Silently skips corrupted entries
    };
```

A malformed index entry (common in a dirty volume) silently disappears from listings instead of surfacing an error.

**Fix:** Surface errors through `set_error` and return `NULL` from `dir_next`, or add a "corrupted" flag to the iterator.

---

### 6. No Validation of Record Number Bounds

**Location:** `src/mft_io.rs:89-91`

```rust
pub fn mft_record_offset(params: &BootParams, record_number: u64) -> u64 {
    params.mft_lcn * params.cluster_size + record_number * params.file_record_size
}
```

No check that `record_number` is within the volume's actual MFT size. Passing an out-of-bounds record number will read/write arbitrary disk locations.

**Fix:** Add bounds checking against `$MFT` data length.

---

### 7. Non-Atomic Read-Modify-Write in `update_mft_record`

**Location:** `src/mft_io.rs:263-289`

The `update_mft_record` function reads the record, applies fixup, calls the mutator, re-applies fixup, then writes back. This is not atomic—another writer (or the same process in a multithreaded context) could modify the record between read and write, causing torn updates.

**Fix:** Add advisory file locking or document that concurrent writes to the same volume are UB.

---

## Minor Issues / Code Quality

### 8. Dead Code / Unused Imports

- `src/bitmap.rs:322-327`: `_touch_unused_imports()` function silences warnings instead of actually using the import
- `src/bitmap.rs:58-59`: Unused `total_clusters` variable (commented out with "silence unused warn")

### 9. Inconsistent Error Handling Patterns

Some C ABI functions check for null pointers individually; others use `cstr_to_path` helper. Standardize on one pattern.

### 10. Clippy Lint Suppression Without FIXME

`src/lib.rs:12` suppresses `clippy::not_unsafe_ptr_arg_deref` with explanation but no FIXME marker for eventual removal.

---

## Design / Architecture Issues

### 11. WOF (Windows Overlay Filter) Not Supported

Files with `IO_REPARSE_TAG_WOF` read as empty/zero because `WofCompressedData` stream is not decompressed. This affects modern Win10/11 volumes where system files are WOF-compressed. STATUS.md §4.4 documents this as "silent data loss on every modern volume."

### 12. Eager Directory Materialization

`src/lib.rs:701-738` still materializes entire directories into `Vec<FsNtfsDirent>`. For directories with 100k+ entries (e.g., `WinSxS`), this uses ~27MB memory and stalls enumeration.

### 13. No `$LogFile` Replay for Dirty Volumes

The driver parses cleanly-unmounted volumes only. Dirty volumes may fail or return stale data. `fs_ntfs_clear_dirty` and `fs_ntfs_reset_logfile` exist for recovery, but there's no warning when mounting a dirty volume.

---

## Suggested Immediate Changes (Priority Order)

| Priority | File | Change |
|----------|------|--------|
| High | `src/lib.rs:288-294` | Add overflow-checked timestamp conversion; widen to 64-bit + nanoseconds for ABI v2 |
| High | `include/fs_ntfs.h:53` | Widen `name[256]` → `name[1024]` |
| High | `src/lib.rs:156` | Document `Send` contract or add runtime check |
| Medium | `src/write.rs:492-494` | Use `checked_sub`/`saturating_add` for VCN calculations |
| Medium | `src/lib.rs:709-717` | Surface index iteration errors instead of `continue` |
| Medium | `src/mft_io.rs:263-289` | Add advisory file locking for RMW operations |
| Low | `src/bitmap.rs:58-59` | Remove dead code / fix the `total_clusters` calculation |
| Low | `src/lib.rs:12` | Add FIXME comment for clippy lint removal |

---

## Testing Gaps

1. **No C-ABI coverage for write operations** — `tests/capi_*.rs` exists for `fsck`, but write tests only exercise the Rust API directly
2. **No tests for >256 byte filenames** — Would catch the dirent truncation bug
3. **No tests for pre-1970 or post-2106 timestamps** — Would catch Y2038 bugs
4. **No tests for concurrent writes** — RMW races would be caught
5. **No tests for WOF-compressed files** — Modern Windows volumes require this

---

## References

- [MS-FSCC](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-fscc/)
- [Flatcap NTFS Documentation](https://flatcap.github.io/linux-ntfs/ntfs/)
- [MS-DTYP FILETIME](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-dtyp/2c57429b-fdd4-488f-b5fc-9e4cf020fcdf)
- STATUS.md §1.1, §1.4, §4.3, §4.4

---

*Review generated by Cascade AI — 2026-04-19*

---

## Maintainer response — 2026-04-19

Walked each point against current code. Baseline is 320 tests passing.
Summary: **agree on 7, push back on 4, fix 1, defer 1.**
(Numbering below mirrors the review.)

### §1 Timestamp truncation — **AGREE**

Correct. `as u32` wraps post-2106; `saturating_sub` only saves us on
the pre-1970 side. Sub-second precision is dropped. Already tracked
as NEXT_PLAN §1.3. Fix is an ABI break (widen `FsNtfsAttr` times to
`int64_t` + `uint32_t` nsec); bundling with §1.4 and §4.3 for a
single-version break. **No NTFS-format change** — on-disk FILETIME
is still u64.

### §2 Dirent `name[256]` buffer — **AGREE**

Correct. NTFS allows 255 UTF-16 code units → up to 1020 UTF-8 bytes.
256-byte `name` silently truncates. Tracked as NEXT_PLAN §1.4,
bundled with §1.

### §3 `unsafe impl Send for CallbackReader` — **AGREE (docs only)**

Valid concern for thread-confined Swift contexts (e.g. `@MainActor`).
Action: expand the Safety comment to explicitly document the contract
the Swift caller must uphold. Not a functional fix. No NTFS impact.

### §4 `new_size - 1` underflow in `grow_nonresident` — **DISAGREE**

The reviewer missed the preceding guard at
[src/write.rs:477](src/write.rs#L477):

```rust
if new_size <= current_len {
    return Err(format!("grow: new_size {new_size} not greater than current {current_len}"));
}
```

`current_len: u64 >= 0`. If `new_size == 0`, `new_size <= current_len`
is always true and we return early before reaching
`new_size - 1`. Adding `checked_sub` here is defensive noise that
obscures the invariant. No change.

### §5 Silent error swallow in dir iteration — **AGREE (minor)**

Current behavior (skip-on-error, continue listing) is deliberate: a
malformed entry shouldn't make an entire dir unreadable, especially
during FSKit enumeration. But we could surface a "n entries skipped"
counter on the iterator struct for diagnostics. Not a must-fix; filed
as a polish item. No NTFS impact.

### §6 No bounds check in `mft_record_offset` — **PARTIAL DISAGREE**

`mft_record_offset` is a pure math helper (line offset = `mft_lcn *
cluster_size + record_number * record_size`). The claim "reads/writes
arbitrary disk locations" is **wrong** — we only read/write the image
file, not raw disk. A bogus record_number produces a bogus file
offset, which `File::seek + read_exact` rejects as EOF. So the failure
mode is "confusing error message", not "memory safety".

Real bounds checking would need to query `$MFT`'s `value_length` per
call, which is expensive. Better addressed at public-API entry
points, which already resolve by path (not raw record number). No
change for now.

### §7 Non-atomic RMW in `update_mft_record` — **AGREE (docs)**

Correct: we don't lock. But:
- fs-ntfs doesn't spawn threads internally; in-process concurrency
  isn't a hazard.
- External concurrency (Windows / ntfs-3g) can't be prevented by
  advisory locks even if we held them.
- Cargo test workers each use their own `_xxx.img` copy, so tests
  don't race.

Action: add a module-level doc comment on `update_mft_record` saying
"concurrent writers to the same image are UB; mount the image
read-only or serialize externally". No code change, no NTFS impact.

### §8 Dead code — **AGREE (fixed now)**

`_touch_unused_imports()` and the `total_clusters = params.
file_record_size; let _ = total_clusters;` placeholder in
`locate_bitmap` were artifacts of earlier refactors. Removed in this
commit. No NTFS impact.

### §9 Inconsistent error handling — **DISAGREE**

`cstr_to_path` is used 61 times in `src/lib.rs`. The pattern IS
consistent. The reviewer may have spotted one of the handful of
spots that unwrap `CStr::from_ptr` directly (where the arg is a
bytestring not a path, like EA name). That's intentional: paths go
through `cstr_to_path`, byte buffers don't. No change.

### §10 Clippy lint no FIXME marker — **DISAGREE**

The `#![allow(clippy::not_unsafe_ptr_arg_deref)]` has a 5-line prose
comment explaining exactly when it should come off (when we bundle
ABI-break §1.3+§1.4+§4.3). A literal `FIXME:` tag adds nothing a
grep for the lint name wouldn't find. No change.

### §11 WOF not supported — **AGREE**

Known. Tracked as NEXT_PLAN §3.8. Requires either `ms-compress`
integration or a from-scratch XPRESS4K/8K/16K + LZX decoder. Big
piece of work. No NTFS-write impact (WOF is a read-path concern).

### §12 Eager directory materialization — **AGREE**

Known. Tracked as NEXT_PLAN §2.5. Lifetime plumbing for a streaming
iterator over the C-ABI is awkward but doable. No NTFS impact.

### §13 No dirty-mount warning — **PARTIAL DISAGREE**

We ship `fs_ntfs_is_dirty` as an opt-in probe. Auto-warning on mount
would change the quiet-by-default contract FSKit relies on. The
driver does parse dirty volumes — it may return stale data but
doesn't panic. Action: document on `fs_ntfs_mount` that callers
should call `fs_ntfs_is_dirty` and dispatch appropriately. No code
change.

### Testing gaps

1. **"No C-ABI coverage for write operations"** — **STALE**. 42 capi
   tests were added for W3/W4 writes in commit 88eecb3:
   `capi_create.rs`, `capi_remove.rs`, `capi_rename.rs`, `capi_ea.rs`,
   `capi_reparse.rs`, `capi_ads.rs`, `capi_write_variants.rs`, plus
   `capi_link.rs` (4 more) and `capi_write_w1.rs`, `capi_write_content.rs`,
   `capi_write_truncate.rs`, `capi_fsck.rs` from earlier. The reviewer
   seems to have looked at a pre-§5.1 snapshot.
2. **>256-byte filenames** — valid. Can't write a fixture until the
   buffer is widened (§2); test bundles with that ABI break.
3. **Pre-1970 / post-2106 timestamps** — valid. Bundles with §1.
4. **Concurrent writes** — see §7; we don't support it and adding a
   test would confirm the UB, not prevent it.
5. **WOF** — valid, requires the feature (§11) first.

### Format-compliance note

The user's constraint is that any fix must preserve valid NTFS
on-disk format. All §1 / §2 changes listed above are pure FFI/ABI
reshaping — the u64 FILETIME and UTF-16 name length 255 on disk are
unchanged. Dead-code cleanup in §8 touches only internal Rust state.
No fix discussed here would emit non-compliant NTFS structures.
