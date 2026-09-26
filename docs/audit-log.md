# Write-path audit log

This log records passes over the mutating NTFS paths with one question:

> Can an edit be applied to a filesystem state that the driver has misread?

Each pass names its source baseline, every module examined, findings, and any
deliberate omissions. A clean module is recorded explicitly; silence is not a
result.

## 2026-09-23 — complete write-path pass

Baseline: `origin/main` at `e718475`, plus the two fixes recorded below. The
pass followed the corruption-first question in `docs/driver-parity-plan.md`
and rechecked the findings already attached to issue
[#135](https://github.com/christhomas/rust-fs-ntfs/issues/135).

| Path | State read before mutation | Result |
| --- | --- | --- |
| `src/write.rs` | file records, `$DATA`/ADS run lists, `$FILE_NAME`, `$EA`, reparse/object IDs, `$VOLUME_NAME`, parent `$I30`, allocation results | Two findings, fixed below. Previously repaired write ordering, rollback, transform guards, initialized length, protected-metafile ranges, and run bounds were rechecked. |
| `src/index_io.rs`, `src/idx_block.rs` | resident `$INDEX_ROOT`, non-resident `$INDEX_ALLOCATION`, `$BITMAP`, collation keys and index-node headers | No new finding. Header fields are bounded by the containing attribute/block, entry offsets must land on boundaries, interior nodes are refused where unsupported, and block transfers must fit one mapped run and the device. |
| `src/attr_io.rs`, `src/attr_resize.rs` | resident and non-resident attribute headers, value extents, mapping pairs and record capacity | No new finding. Edits remain inside the selected attribute/record and reject malformed ranges before splice, resize, promotion, or commit. |
| `src/data_runs.rs`, `src/sparse.rs` | mapping pairs, VCN spans, sparse holes and allocated cluster extents | No new finding. Arithmetic is checked, headers derive from encoded runs, and allocated extents are device-bounded before publication. |
| `src/bitmap.rs`, `src/mft_bitmap.rs` | `$Bitmap` and `$MFT::$BITMAP` bytes, cluster/MFT capacity and allocation hints | No new finding. Each allocation reads the committed bitmap state, validates capacity, and rolls partial writes back; sequential allocations cannot spend the same stale bit. |
| `src/mft_io.rs` | boot geometry, `$MFT` runs, FILE headers, update sequence/fixups and mirror extent | No new finding. Record updates re-read and validate the current record, restore updates advance the USN, and mirror synchronization is bounded to records 0–3. |
| `src/fsck.rs` | `$Volume`, `$LogFile`, record/fixup state and protected metadata extents | No new finding. `$Volume` mutations already synchronize record 3 to `$MFTMirr`; logfile writes use checked target ranges. |
| `src/ea_io.rs`, `src/record_build.rs` | EA entry lengths/counts and generated resident/non-resident attribute sizes | No new corruption finding. The separately tracked `$EA_INFORMATION` layout work was not duplicated by this pass. Builders size and align output before copying it into a record. |
| `src/facade.rs`, `src/lib.rs`, `src/bin/` | path/handle/FFI arguments | No independent write implementation and no new finding; these entry points validate arguments and delegate to the audited writers. |
| `src/mkfs.rs` | format parameters and fixed construction geometry | No edit-after-misread path: this constructs a new image. Its direct writes and mirror geometry were checked for overlap with the audited helpers; no new finding. |

### Findings

1. Variable-length rename used the referenced MFT record number to decide
   whether a destination collision was the source entry. Two hard links in
   the same directory share that number, so renaming one to the other's name
   inserted a duplicate `$I30` collation key. The collision check now compares
   the source and destination index-entry offsets, matching the repaired
   same-length path. Regression:
   `rename_variable_onto_other_hard_link_is_refused`.
2. `set_volume_label_io` committed record 3 (`$Volume`) but did not refresh
   the copy in `$MFTMirr`. A successful label change therefore left the
   primary and recovery records inconsistent. The writer now calls
   `sync_mftmirr_record_io` after the primary commit. Regression:
   `changing_the_volume_label_keeps_the_mirror_in_step`, covering both label
   insertion and removal.

### Previously recorded findings rechecked by this pass

The earlier portions of the same audit were not preserved in a dated ledger.
Their linked findings are recorded here so the pass is complete rather than
only listing today's delta:

- [#140](https://github.com/christhomas/rust-fs-ntfs/issues/140): same-length
  rename rollback and commit ordering.
- [#141](https://github.com/christhomas/rust-fs-ntfs/issues/141): unlink freed
  clusters before retiring the record.
- [#142](https://github.com/christhomas/rust-fs-ntfs/issues/142): replacing or
  deleting a non-resident ADS leaked its runs.
- [#146](https://github.com/christhomas/rust-fs-ntfs/issues/146): failed grow
  leaked its allocation.
- [#147](https://github.com/christhomas/rust-fs-ntfs/issues/147): empty
  resident promotion disagreed with its own run list.
- [#148](https://github.com/christhomas/rust-fs-ntfs/issues/148): raw LCN
  multiplication bypassed the checked volume span.
- [#157](https://github.com/christhomas/rust-fs-ntfs/issues/157): write guards
  did not cover all protected system-metafile extents.
- [#164](https://github.com/christhomas/rust-fs-ntfs/issues/164): writes past
  `initialized_length` reported success but read back as zeroes.
- [#168](https://github.com/christhomas/rust-fs-ntfs/issues/168): INDX insertion
  accepted an unsupported interior node.
- [#169](https://github.com/christhomas/rust-fs-ntfs/issues/169): INDX insertion
  trusted unbounded header offsets and sizes.
- [#170](https://github.com/christhomas/rust-fs-ntfs/issues/170): sparse and
  encrypted write flags were read with the wrong mask.
- [#171](https://github.com/christhomas/rust-fs-ntfs/issues/171): lookup and
  insertion used different filename collation.
- [#218](https://github.com/christhomas/rust-fs-ntfs/issues/218): deep
  zero-length writes advanced metadata and could zero-fill existing data.
- [#219](https://github.com/christhomas/rust-fs-ntfs/issues/219): the
  same-length rename collision check confused two hard-link entries.
- [#242](https://github.com/christhomas/rust-fs-ntfs/issues/242): restored MFT
  records reused the on-disk USN and could mask a torn restore.
- [#193](https://github.com/christhomas/rust-fs-ntfs/pull/193): an
  `$ATTRIBUTE_LIST` was consulted too late, after the base-record read had
  already returned a partial logical attribute.

### Deliberate exclusions

- Concurrent external writers are outside the unmounted-image writer's
  contract. Re-reading inside a single operation is still required and was
  audited; no process-level locking guarantee is inferred here.
- Device-I/O failure atomicity was inspected where it intersects allocation
  rollback, but this pass does not claim journalling or crash-atomic
  transactions.
- Existing work already claimed under other issues, including overflow-index
  routing and `$EA_INFORMATION` layout, was noted but not changed or
  duplicated.

### Windows / `chkdsk` evidence classification

These are on-disk write-path changes, so the Windows oracle applies before
merge. The repository's full matrix is the broad compatibility gate. Exact
defect evidence is:

1. On a fresh image, create one file and a second hard link in the same
   directory using a name of different UTF-16 length; attempt a non-replacing
   rename of the first name onto the second. The call must report an existing
   destination, enumeration must still contain each original name exactly
   once, and `chkdsk` read-only plus `/scan` must report clean.
2. On a fresh labelled image, change the label and then remove it through the
   driver. After each operation, raw FILE records 3 in `$MFT` and `$MFTMirr`
   must be identical after USA fixup, Windows must report the expected label,
   and `chkdsk` read-only plus `/scan` must report clean.

The local Rust regressions prove the identity/no-mutation and mirror-byte
conditions. A generic matrix seal proves no broad compatibility regression;
it does not replace these two targeted operation sequences if the matrix does
not execute them.
