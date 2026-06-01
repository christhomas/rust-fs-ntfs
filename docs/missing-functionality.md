# Missing Functionality / API Gaps

A running log of capabilities the write API does **not** yet have — either
things that block a test from being written at all, or behaviors that
diverge from NTFS such that a test exists but is `#[ignore]`d pending a fix.

Discovered during the test-expansion effort (see
`docs/test-expansion-plan-2026-06-01.md`). Append new findings; don't rewrite
history.

---

## Status (2026-06-01, verified against current code)

**Resolved (implemented + tested):**
- **B1** `unlink` decrements `hard_link_count` — `src/write.rs` `unlink_io` (4279e8a).
- **B2** `rename` rejects an existing destination — `src/write.rs` (4279e8a).
- **B3** resident-ceiling rejection — was a *test* bug, fixed; the capacity guard already lived in `resize_resident_value`.
- **C6** POSIX-style `remove` that dispatches by type — `src/write.rs` `remove`/`remove_io` + `rust_ntfs remove` (4279e8a/f92e78b).
- **C2** `truncate` grow (extend) — routed to the existing `grow_nonresident` allocation path; no new on-disk behavior.

**Still open (genuinely unimplemented — confirmed present in code):** A1 sparse-write, A2 `$INDEX_ALLOCATION` *growth* (insertion into existing INDX blocks works, but no new-block allocation / B-tree split), A3 compressed-write, A4 new security-descriptor authoring, B4 `$FILE_NAME` duplicate fields (deferred pending chkdsk), C1 resident in-place write/grow, C3 non-resident `$EA`/`$REPARSE_POINT`/`$Bitmap:$I30`, C4 compressed-read, C5 case-sensitive collation wiring.

**Why the rest can't be done in this sandbox:** each remaining item changes on-disk *write* structure (sparse runs, B-tree blocks, compression streams, `$SDS` hash trees, non-resident attribute conversion) and must be validated against Windows chkdsk via the 42-scenario matrix (`windows-test-matrix`) before it's safe to land — that VM/chkdsk loop isn't available here, and these are instance 2's active write-path development area. C5 is additionally blocked on the unpinned `FILE_ATTRIBUTE_CASE_SENSITIVE_DIR` bit (needs a byte-diff against a real WSL volume). C2 was the one exception: it reuses already-validated machinery, so it carried no new on-disk risk.

Each resolved item is struck through (`~~…~~`) in place below; open items are unchanged.

---

## A. Blocking gaps — no API exists, so the scenario can't be tested at all

### A1. Sparse-file write / punch-hole
- **What's missing:** No public API to create a sparse file, write with holes,
  or punch a hole. Sparse handling is **read-only** (the existing `sparse.rs`
  test reads a prebuilt fixture).
- **Blocks:** Plan Phase 3.5 (sparse semantics) cannot be self-generated.
- **Foundation already present:** `data_runs.rs` fully models holes
  (`DataRun.lcn: Option<u64>`, `None` = hole; `encode_runs`/`decode_runs`
  round-trip them) and the read path handles them (`vcn_to_lcn`,
  `range_has_hole_or_past_end`). The non-resident header size fields
  (allocated/data/initialized at 0x28/0x30/0x38) are already manipulated by
  `write.rs`.
- **Still needed:** (1) set the SPARSE attribute data-flag `0x8000` (attr
  header +0x0C) and `FILE_ATTRIBUTE_SPARSE_FILE` `0x200`; (2) a
  `write_sparse`/`punch_hole` API that represents zero regions as `lcn=None`
  runs without allocating clusters; (3) `allocated_size` accounting that
  counts only real (non-hole) clusters; (4) chkdsk-exact invariants.

### A2. `$INDEX_ALLOCATION` overflow (large directories)
- **What's missing:** A directory's `$INDEX_ROOT` cannot overflow into
  `$INDEX_ALLOCATION` on the write path — there's no B-tree split / index-block
  allocation. **Empirically, `create_file` fails after ~22 entries** in one
  directory (4096-byte records; `tests/directory_scaling.rs` probe).
- **Blocks:** Plan Phase 3.4 (large directories: 100 / 1000 / 10000 entries).
  The existing `manyfiles.rs` (512 files) / `deep.rs` only work against
  prebuilt fixtures, not our write path.
- **Note:** entries up to the ceiling are correct + findable + sorted (no
  silent loss); the operation cleanly errors at the ceiling.

### A3. Compressed-stream write
- **What's missing:** No write path for NTFS-compressed `$DATA`. Read-only
  (upstream limitation).
- **Blocks:** any compression round-trip test.

### A4. New security-descriptor creation
- **What's missing:** Write can only point a file's `security_id` at an
  existing `$Secure:$SDS` entry (the canonical `0x100` system-files DACL).
  There's no API to author a *new* SD and append it to `$SDS`/`$SDH`/`$SII`.
- **Blocks:** Plan Phase 3.8 beyond "default SD on creation" — can't test
  custom ACLs, inheritance, per-file owners.

---

## B. Behavioral gaps — test written, currently `#[ignore]`d pending a fix

These are in `src/write.rs`. Each has a live `#[ignore]`d test that flips to
passing once the behavior is fixed.

### ~~B1. `unlink` does not decrement `hard_link_count`~~ ✅ RESOLVED (4279e8a)
~~- **Symptom:** After `unlink` of one of N hard-linked names, the name becomes
  unfindable (correct) but the FILE record header's `hard_link_count` stays N
  (should be N-1). `link` *does* increment correctly.~~
- **Fix:** `unlink_io` now reads `hard_link_count` (rec +0x12) and, when > 1,
  drops only the matching `$FILE_NAME` and decrements the count — record/clusters
  freed only on the last link. `unlink_decrements_hard_link_count` passes.

### ~~B2. `rename` onto an existing name does not error~~ ✅ RESOLVED (4279e8a)
~~- **Symptom:** `rename(src, existing_dst)` succeeds instead of failing.~~
- **Fix:** the rename path now checks the destination in both the resident
  `$INDEX_ROOT` and any `$INDEX_ALLOCATION` INDX blocks and returns
  `'<name>' already exists`. `rename_onto_existing_name_errors` passes.

### B3. `write_resident_contents` has no upper-bound check
- **Symptom:** Does not reject a payload one byte over the resident ceiling
  (`src/write.rs` ~2900); should `Err` so the caller promotes to non-resident.
- **Test:** `tests/boundary_sizes.rs::one_over_ceiling_rejected_by_resident_path`
  (file held uncommitted pending this fix). *(Owned by instance 1.)*

### B4. `$FILE_NAME` duplicated size/attr fields not refreshed on write
- **Symptom:** `write_file_contents` does not update
  `$FILE_NAME.data_size`/`allocated_size`; `set_file_attributes` does not
  update `$FILE_NAME.file_attributes` (only `$STANDARD_INFORMATION`).
- **Caveat:** This may MATCH Windows, which refreshes the `$FILE_NAME`
  duplicate fields lazily (on rename/close). **Do not "fix" without Windows
  VM + chkdsk confirmation** — it could be correct as-is.
- **Tests:** `tests/field_exhaustion_fn.rs` (3 `#[ignore]`d).

---

## Appended by instance 1 (2026-06-01, session 93c01079)

### Correction to B3 — RESOLVED, was a test bug not a code gap
`write_resident_contents` does **not** need its own upper-bound check: the
rejection happens one level down in `attr_resize::resize_resident_value`'s
capacity guard (`growing attribute by N bytes exceeds record capacity`). The
original `boundary_sizes.rs` failure was a **test** bug — it computed the
ceiling with a wrong/shared probe. After the probe was rewritten to measure
the real per-file ceiling empirically, `ceiling+1` is correctly rejected.
`tests/boundary_sizes.rs` is now green (single-threaded **and** parallel) and
**committed** (`4a942b9`). No production code change was required.

### A2 refinement — measured ceilings
The large-directory ceiling is **per-directory and name-dependent**: ~**24
entries in the root**, ~**36 in a fresh subdirectory** (4 KiB records), not a
single global number. See `tests/large_directory.rs` (committed `93560b5`),
which covers correctness + sorted order up to the ceiling and graceful failure
past it.

### C. Additional blocking gaps found (no API / refused on write)

#### C1. Resident `$DATA` in-place write / grow / truncate
The size-mutating write paths handle **non-resident** `$DATA` only; a
freshly-created (resident) file must be promoted first.
- `src/write.rs:452` — `write_at only supports non-resident $DATA in W1`.
- `src/write.rs:594` — `truncate: resident $DATA unsupported in W2 MVP`.
- `grow_nonresident_by_record_number_io` — `refusing resident $DATA (use W2.2
  promotion)`.
- **Workaround for tests:** call `promote_resident_data_to_nonresident` first.

#### ~~C2. `truncate` grow (extend)~~ ✅ RESOLVED
~~`truncate` shrinks only — `truncate: grow not yet implemented`.~~
**Fix:** `truncate_by_record_number_io` now routes the `new_size > current_len`
case to `grow_nonresident_by_record_number_io` (existing, matrix-validated
allocation) — same on-disk result, no new behavior. Tests
`truncate_grow_extends_nonresident_file` + `truncate_grow_to_same_size_is_noop`.
(Still resident-only-via-promote, same as C1.)

#### C3. Non-resident forms of metadata attributes (resident-only MVP)
- `$EA` — `src/ea_io.rs:128`: `$EA is non-resident (MVP only supports resident
  EAs)`. Blocks large-EA-set tests.
- `$REPARSE_POINT` — `src/write.rs:1657`: `$REPARSE_POINT is non-resident (not
  yet supported)`. Blocks large reparse-buffer tests.
- `$Bitmap:$I30` — `src/idx_block.rs:111`: `non-resident $Bitmap:$I30
  unsupported in this MVP`. Ties into A2 (large dirs).

#### C4. Compressed-file read (decompression)
- `src/lib.rs:1817` — `file is compressed (LZNT1); decompression not yet
  supported`.
- `src/lib.rs:1846` — `file is WOF-compressed (IO_REPARSE_TAG_WOF);
  decompression not yet supported`.
Blocks read-back of compressed files. (Complements A3, which is the write side.)

#### C5. Case-sensitive directory collation not wired in
`compare_names_ordinal` exists but is **not** used by `find_index_entry` or the
insert paths (always case-insensitive); the
`FILE_ATTRIBUTE_CASE_SENSITIVE_DIR` bit is unpinned. `src/index_io.rs:683,691`.
Blocks WSL/Docker case-sensitive-dir scenarios.

#### ~~C6. `unlink` on directories~~ ✅ RESOLVED (4279e8a / f92e78b)
~~`unlink` refuses directories; directory removal goes through `rmdir`.~~
**Fix:** added `write::remove`/`remove_io` (and the `rust_ntfs remove` CLI
subcommand) — a POSIX-style remove that dispatches to `unlink` for files and
`rmdir` for directories. (`unlink` itself remains file-only by design.)
