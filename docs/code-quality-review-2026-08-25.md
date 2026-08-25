# Code quality review — 2026-08-25

**Scope:** `src/`, 17,287 production lines across 35 files (test modules excluded from
every count below).
**Findings:** 3 high, 3 medium, 2 low. No fixes applied — this is a read of the code
as it stands.

This crate and `rust-fs-ext4` are the two largest in the family, and they show the
same shape of wear in the same places: the write and format paths have grown by
accretion, while the read path has stayed comparatively tidy. Nothing here is a
correctness finding. Every item is about how long it takes a reader to establish
that the code is correct.

---

## H1 — `mkfs::format_filesystem` is 1,141 lines

**`src/mkfs.rs:271`**

One function, an eighth of the file it lives in, doing every part of laying down a
filesystem. It is not disorganised — it is carefully ordered, and it tells you so:

```rust
// 0. Zero out critical regions ---------------------------------------
// 1. Boot sector + backup --------------------------------------------
// 2. $LogFile — write 12 KiB of canonical RSTR + RCRD pages
// 3. $UpCase data -----------------------------------------------------
// 4. $Bitmap data -----------------------------------------------------
// 5. MFT records ------------------------------------------------------
```

Those six comments are the finding. A function that has to number its own steps has
already been decomposed by whoever wrote it; the decomposition just was not expressed
in the language. Each numbered section is a named function with a clear input and a
clear output, and the numbering shows exactly where the boundaries are.

**Why it matters here specifically.** This function is where the hardest-won knowledge
in the crate lives — the comments record chkdsk Event ID 55, what Microsoft's
`format.com` writes for `$Bad`'s sparse run, a procmon trace showing which name the
Windows driver opens, and dated notes from individual debugging iterations. That
material is worth more than the code around it. Today a reader looking for "how do we
build `$UpCase`" has to scroll 1,141 lines to find out whether they have seen all of
it. Extraction is what makes each of those notes findable from the thing it explains.

**Shape of the fix.** One function per numbered section, each taking the layout
decisions it needs and returning the bytes or records it produces. The section
comments become the doc comments. Nothing else moves.

---

## H2 — `write.rs` is 3,710 lines and `lib.rs` is 3,245

**`src/write.rs`, `src/lib.rs`**

`lib.rs` is a crate root that is also a 3,245-line implementation file. A crate root
is the first thing anyone opens, and it should say what the crate is made of; this one
requires reading it to find out.

`write.rs` holds the whole mutation surface, including four of the ten longest
functions in the crate (`write_sparse_file_io` 158, `grow_nonresident_by_record_number_io`
126, `truncate_by_record_number_io` 121, and others). The file names a broad concern
rather than a specific one, which is what lets it keep growing — there is no size at
which "does this belong in write.rs?" answers itself.

**Shape of the fix.** Split by the structure being written rather than by the fact of
writing: resident attributes, non-resident runs, sparse regions, index updates. Move
the C ABI out of `lib.rs` and leave the root as module declarations and the crate's
documentation.

---

## H3 — 112 unnamed multi-digit offsets, 68 of them in one file

**`src/record_build.rs` (68), `src/mkfs.rs` (39), `src/write.rs` (5)**

A raw offset in a parse or build expression gives a reader no way to tell a correct
value from a typo. They cannot check `record[56]` against the format documentation
without counting fields; they can check `record[REC_OFF_BYTES_USED]` by eye.

The crate already knows this — `attr_resize.rs` uses `REC_OFF_BYTES_USED` and
`REC_OFF_BYTES_ALLOCATED` — so this is an inconsistency rather than an unmade decision.
`record_build.rs` is where the convention lapsed most.

**Shape of the fix.** An `offsets` module per structure, naming every field, including
the ones nothing currently reads: an offset is only checkable against the specification
when its neighbours are there to count off against.

---

## M4 — The same field read is written out three times

**`src/attr_resize.rs:62`, `:156`, `:220`**

```rust
let bytes_used = u32::from_le_bytes([
    record[REC_OFF_BYTES_USED],
    record[REC_OFF_BYTES_USED + 1],
    record[REC_OFF_BYTES_USED + 2],
    record[REC_OFF_BYTES_USED + 3],
]) as usize;
```

Three identical copies, each also reading `bytes_allocated` the same way — 23
duplicated eight-line blocks across the crate, 51 occurrences in total, concentrated
here.

Nothing forces the three to agree. Naming the constants was the first half of the job;
`fn le_u32_at(record: &[u8], off: usize) -> u32` is the second, and it removes the
`+ 1 / + 2 / + 3` arithmetic that is the actual place a mistake would hide.

---

## M5 — 46 functions of 60 lines or more

**crate-wide**

Beyond `format_filesystem`, the tail is long: `build_attrdef_table` (167),
`build_system_record_with_parent` (155), `write_sparse_file_io` (158). Several mix
abstraction levels — byte assembly next to allocation policy next to error mapping —
which is what makes them tiring rather than merely long.

This is a consequence of H1 and H2 rather than a separate problem, and it will shrink
as those are addressed. Worth tracking as a number so it can be watched going down.

---

## M6 — 43 functions take five or more parameters

**`src/index_io.rs:build_file_name_index_entry` (5), `src/lib.rs:fs_ntfs_set_times` (6),
and 41 others**

Some are unavoidable: a C ABI entry point takes what the ABI says it takes, and those
should be left alone. The internal ones are usually a struct that has not been named
yet — several pass the same four or five layout values together, which is a
`VolumeLayout` waiting to be extracted.

Worth separating the two groups before acting on either.

---

## L7 — Seven `#[allow(...)]` with no stated reason

**crate-wide**

`#[allow(dead_code)]` and `#[allow(clippy::type_complexity)]` appear without a comment
saying why the lint is wrong here. Each is probably justified; none of them says so, so
a later reader cannot tell a deliberate suppression from one added to get a build
green.

One line above each is enough.

---

## L8 — 73 lines indented 24 columns or deeper

**crate-wide**

Six levels of nesting or more. Not the worst in the family (`rust-fs-ext4` has 271),
and concentrated in the same long functions as everything else, so it will largely
resolve with H1 and H2.

---

## What is good, and should survive any refactor

- **The comments explain *why*.** `mkfs.rs` in particular records what Windows does,
  what chkdsk complains about, and what was tried and rejected — with dates. This is
  the most valuable material in the crate and the hardest to reconstruct. Any
  extraction must carry it along rather than summarise it.
- **The read path is in much better shape than the write path.** The parsing modules
  are focused and reasonably sized.
- **No duplicated logic across module boundaries.** What duplication exists is local
  and mechanical, which is the easy kind.
- **`clippy -D warnings` and `rustfmt` are clean**, and CI enforces both.

## Suggested order

H3 first: it is mechanical, it is safe, and it makes the code that H1 and H2 will move
readable before it is moved. Then H1, one numbered section at a time. H2 last, since
the file boundaries are easier to choose once the functions inside them are smaller.

M4 can be done at any point and takes minutes.
