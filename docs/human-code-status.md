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

### B3, B4 — ten hand-written `free_all` rollbacks in one 164-line function, and a rename that commits step 1 then runs step 2 with no rollback

**B3 and B4 are the pair worth reading together.** One function writes its
rollback out ten times and is currently correct; another, four hundred lines
away, omits it entirely and leaves a torn rename. Because neither is a named,
shared idiom, the divergence is invisible — which is the same shape as B2, and
the reason B2 was worth fixing by extraction rather than by patching the fifth
copy.

### The remainder

A1–A13 (the CLI), the rest of B, C, D (`mkfs.rs`), E, F and G are recorded in
the report with locations and coverage notes.

---

## Verification

583 unit tests pass, up from 580. `chore lint` clean.

The 63 locally-failing test binaries are pre-existing missing `test-disks/*.img`
fixtures — there is no `mkntfs` on macOS. CI generates them.
