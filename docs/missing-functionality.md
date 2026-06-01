# Missing Functionality / API Gaps

Status of write-API capabilities surfaced during the test-expansion effort
(see `docs/test-expansion-plan-2026-06-01.md`). Each gap either blocks a test
from being written or marks a behaviour that diverges from NTFS.

**Last verified:** 2026-06-01, against the current `main` + the write-path /
CLI work (PRs #54–#57).

---

## Fixed

| ID | Gap | How it was resolved |
|----|-----|---------------------|
| B1 | `unlink` didn't decrement `hard_link_count` | `unlink_io` reads the count (rec +0x12); when > 1 it drops only the matching `$FILE_NAME` and decrements, freeing the record/clusters only on the last link. Test: `unlink_decrements_hard_link_count`. (4279e8a) |
| B2 | `rename` onto an existing name didn't error | Rename path checks the destination in both resident `$INDEX_ROOT` and `$INDEX_ALLOCATION` INDX blocks and returns `'<name>' already exists`. Test: `rename_onto_existing_name_errors`. (4279e8a) |
| B3 | `write_resident_contents` "missing" upper-bound check | Not a code gap — the capacity guard already lives in `attr_resize::resize_resident_value`. The original failure was a *test* bug (wrong ceiling probe); fixed by measuring the real per-file ceiling. (PR #54) |
| C6 | No POSIX remove dispatching by type | Added `write::remove` / `remove_io` + the `rust_ntfs remove` CLI subcommand: file → `unlink`, directory → `rmdir`. (`unlink` stays file-only by design.) (4279e8a / f92e78b) |
| C2 | `truncate` couldn't grow (extend) | `truncate_by_record_number_io` routes `new_size > current_len` to `grow_nonresident_by_record_number_io` — the existing, matrix-validated allocation path, so no new on-disk behaviour. Tests: `truncate_grow_extends_nonresident_file`, `truncate_grow_to_same_size_is_noop`. (PR #57) |

---

## Outstanding

All nine remaining gaps change **on-disk write structure** and therefore must
be validated against Windows chkdsk via the 42-scenario matrix
(`windows-test-matrix`) before they can land — that VM/chkdsk loop is not
available in the current sandbox, and the write path is instance 2's active
development area. They are **not** safe to implement blind. Listed by area.

### A. No API exists — scenario can't be tested at all

**A1 — Sparse-file write / punch-hole.**
No public API to create a sparse file, write with holes, or punch a hole;
sparse handling is read-only. Foundation is present (`data_runs.rs` models
holes as `lcn: None`; the non-resident size fields are already manipulated).
*Needs:* set the SPARSE data-flag (`0x8000`) + `FILE_ATTRIBUTE_SPARSE_FILE`
(`0x200`); a `write_sparse`/`punch_hole` API emitting `lcn=None` runs without
allocating clusters; `allocated_size` that counts only real clusters; chkdsk
validation. Blocks Plan 3.5.

**A2 — `$INDEX_ALLOCATION` growth (large directories).**
Inserting into *existing* INDX blocks works, but the write path cannot
**allocate a new INDX block or split** when the index is full, so a directory
caps when its current index space fills (~24 entries in the root / ~36 in a
fresh subdir at 4 KiB records). Entries up to the ceiling are correct, sorted,
and findable; insertion past it errors cleanly (no corruption).
*Needs:* new-block allocation in `$INDEX_ALLOCATION` + `$Bitmap:$I30` update +
B-tree split, with chkdsk validation. Blocks Plan 3.4 (100/1000/10000 entries).

**A3 — Compressed-stream write.**
No write path for NTFS-compressed `$DATA` (LZNT1). *Needs:* a compressor + the
compressed-run on-disk format. Large; chkdsk-gated.

**A4 — New security-descriptor authoring.**
Write can only point a file's `security_id` at an existing `$Secure:$SDS`
entry (the canonical `0x100` system DACL). No API to author a new SD and append
it to `$SDS`/`$SDH`/`$SII`. *Needs:* SD serialisation + `$Secure` hash-tree
insertion, chkdsk-gated. Blocks custom-ACL / inheritance / per-file-owner tests.

### B. Behaviour diverges — `#[ignore]`d test pending a fix

**B4 — `$FILE_NAME` duplicate size/attr fields not refreshed on write.**
`write_file_contents` doesn't update `$FILE_NAME.data_size`/`allocated_size`;
`set_file_attributes` updates only `$STANDARD_INFORMATION`, not the
`$FILE_NAME` copy. **May be correct as-is** — Windows refreshes these lazily
(on rename/close). **Do not "fix" without Windows-VM + chkdsk confirmation.**
`tests/field_exhaustion_fn.rs` carries the `#[ignore]`d cases.

### C. Write refused / not wired

**C1 — Resident `$DATA` in-place write / grow.**
`write_at`, `grow_nonresident`, `truncate` operate on **non-resident** `$DATA`
only; a freshly-created (resident) file must be promoted first
(`promote_resident_data_to_nonresident`). *Needs:* either in-place resident
mutation or transparent auto-promote, chkdsk-gated.

**C3 — Non-resident forms of metadata attributes (resident-only).**
`$EA` (`ea_io.rs`), `$REPARSE_POINT` (`write.rs`), and `$Bitmap:$I30`
(`idx_block.rs`) are only handled when resident. *Needs:* resident→non-resident
conversion for these, chkdsk-gated. (`$Bitmap:$I30` ties into A2.) Blocks
large-EA-set / large-reparse / large-directory-bitmap tests.

**C4 — Compressed-file read (decompression).**
Reading LZNT1- or WOF-compressed file content is unsupported
(`lib.rs`: "decompression not yet supported"). *Needs:* an LZNT1 (and WOF)
decompressor. Complements A3 (the write side).

**C5 — Case-sensitive directory collation not wired in.**
`compare_names_ordinal` exists but isn't used by `find_index_entry` or the
insert paths (lookups/inserts are always case-insensitive). **Additionally
blocked:** the `FILE_ATTRIBUTE_CASE_SENSITIVE_DIR` bit position is unpinned and
needs a byte-diff against a real WSL/Docker volume to determine. Blocks
WSL/Docker case-sensitive-directory scenarios.

---

## How to close an outstanding gap safely

1. Drive the Windows VM per the `windows-test-matrix` skill (the chkdsk oracle).
2. Implement against the matrix, not blind — every on-disk change must pass the
   relevant scenarios.
3. Verify the round-trip with the upstream `ntfs` parser in a self-generated
   test, then re-run the matrix and seal `matrix-results.json`.
4. Move the entry from **Outstanding** to **Fixed** with the commit/PR.
