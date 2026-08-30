# Human-code findings — status

Tracks the **High** and **Medium** findings from
[`human-code-report-2026-08-28.md`](human-code-report-2026-08-28.md). The report
predates the work; this is the current position. Updated 2026-08-30.

**103 findings** — 56 High, 44 Medium, 3 Low, grouped A (CLI), B (`write.rs`),
C (C ABI), D (`mkfs.rs`), E, F, G.

This crate has by far the largest set in the family, and the report's own
ordering is followed: G1 first, then B2.

| | count |
|---|---|
| Fixed | 2 |
| Still open | 98 High/Medium |

---

## Fixed

### G1 — the resident `$MFT:$Bitmap` read a stale snapshot — **fixed earlier**

[#104](https://github.com/christhomas/rust-fs-ntfs/pull/104), and the report
calls it "the most important finding". Reads came from a copy taken when the
attribute was located while writes patched `$MFT`'s record on disk, so a bit set
by `allocate_io` was invisible to the next read, every MFT rollback failed with
"MFT record N already free", and six call sites discarded that error with
`let _ =`.

### B2 — the fifth basename validator accepted `".."` — **fixed**

Five copies of the same check. Four rejected empty, `.`, `..` and `/`. The
fifth — `rename_same_length_io` — tested **only** for a separator and emptiness:

```rust
if new_name.contains('/') || new_name.is_empty() {
```

`".."` is two UTF-16 units, so renaming any two-character name to `".."`
satisfies the same-length rule, passes that check, and reaches the directory
index. The path is public through `facade::rename_same_length` **and** through
the C ABI as `fs_ntfs_rename_same_length`, with no guard downstream.

Now one `validate_basename`, used at all five sites, with tests covering `..`,
`.`, empty, separators, and the names that merely *contain* dots and must still
be allowed (`..a`, `a..`, `...`).

---

## Still open — 98 High and Medium

Not triaged individually here; the report carries the detail. The ones its own
ordering puts next:

### C1 — `fs_ntfs_write_file` and `fs_ntfs_write_file_contents` treat `len == 0` oppositely — High

Two C entry points disagreeing about what an empty write means. A caller cannot
be right about both.

### G3 — a dead assignment in the cluster allocator's wrap-around scan — Medium

The report calls this a two-line deletion.

### G5 — the `$Bitmap` byte-range write half has no all-or-nothing behaviour on a multi-run bitmap — High

A partial failure leaves the bitmap inconsistent with the records it describes.

### B4 — a rename that committed step 1 and then ran step 2 with no rollback — **fixed**

A variable-length rename is two record writes with no journal between them:

1. swap the parent's `$INDEX_ROOT` entry;
2. rewrite the file's own `$FILE_NAME`.

Each `update_mft_record_io` is durable on its own, so **a failure at step 2 left
step 1 standing**: the directory names the new basename while the file's
`$FILE_NAME` still reads the old one. A torn rename, produced silently — the
kind of inconsistency chkdsk reports and this crate would never notice itself.

The bytes needed to undo step 1 were already in hand: `parent_record_bytes` is
read before it and nothing writes to the parent in between. Step 2's failure now
restores them and returns the original error.

If the rollback *also* fails the volume genuinely is torn, and no further write
here can be trusted to repair it, so the error says exactly what is on disk
rather than reporting only the first failure.

`restore_mft_record_io` is the named idiom, in `mft_io.rs` beside the primitive
it undoes. Its doc says what it is not: a record whose previous bytes the caller
still holds, not a general undo, and no help once a second record is committed
too. That naming is the point — B3 and B7 are two more rollback shapes written
out by hand, and the reason this one could go missing is that none of them had a
name to be missing from.

Three tests, over a `BlockIo` that refuses writes to one MFT record — the only
way to fail step 2 without failing the operation earlier. Mutation-checked:
removing the rollback fails
`a_failed_rename_leaves_the_directory_naming_the_old_file` on
`the old name must be back in the directory index`.

### B3, B7 — two more hand-written rollback shapes — **fixable, not yet done**

One 164-line function writes its `free_all` rollback out ten times and is
currently correct; `create_file_io` spells out a third distinct shape. Both are
right today. B4 is what happens when a fourth site forgets, which is the
argument for giving these a name too.

### C1 — two adjacent write entry points meant opposite things by `len == 0` — **documented and pinned**

```c
int64_t fs_ntfs_write_file(image, path, offset, buf, len);
int64_t fs_ntfs_write_file_contents(image, path, buf, len);
```

Both open with `if len == 0`, and mean the opposite by it. The ranged write
returns 0 and does nothing — **without opening the image or resolving the path**,
so a zero-length write to a file that does not exist also returns 0. The
whole-contents write passes `&[]` through, which **empties the file**.

Both are right. "Rewrite this range" with an empty range is a no-op; "write this
as the entire contents" with nothing is `open(O_TRUNC)`. The problem is that
neither is inferable from the signature, and the two sit next to each other with
identical-looking guards — so a caller cannot be right about both.

The no-op is the library's policy at three levels, not a shortcut in the C
wrapper: `write::write_at` and `write_at_io` both return `Ok(0)` for empty data
before resolving anything, so a caller chunking a buffer pays nothing for an
empty tail. **Changing that is a behaviour change on a published ABI with no
reported harm behind it, and stays your call.** What was clearly fixable is that
neither behaviour was written down.

Both now state it, in the Rust doc and in `fs_ntfs.h` where a C caller actually
reads it, each pointing at the other. Three tests pin the pair — including the
zero-length write to a path that does not exist, contrasted with the one-byte
write to the same path that fails — so a later change making them agree in either
direction has to be deliberate.

The tests format their own image rather than using `test-disks/`, which needs
`mkntfs` and so only exists on CI.

### The remainder

A1–A13 (the CLI), the rest of B, C, D (`mkfs.rs`), E, F and G are recorded in
the report with locations and coverage notes.

---

## Verification

586 unit tests pass, up from 583. `chore lint` clean.

The 63 locally-failing test binaries are pre-existing missing `test-disks/*.img`
fixtures — there is no `mkntfs` on macOS. CI generates them.
