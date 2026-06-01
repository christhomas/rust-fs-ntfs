# 03 — The Write Path

> *Reads can be wrong and you lose nothing — re-read and try again. Writes are
> where data is created, moved, and destroyed. This is where the stakes are, and
> where the testing is most aggressive.*

The write path is original work layered on top of the read path. It covers
creating and deleting files and directories, renaming, growing and truncating
content, promoting a file from in-MFT (resident) storage to allocated clusters
(non-resident), hard links, alternate data streams, extended attributes, reparse
points, timestamps, and file-attribute flags.

**Every write test ends by reading the result back** — and, for the structural
tests, reading it back with the *independent* `ntfs 0.4` parser. A write that our
own reader tolerates but that the independent parser (or `chkdsk`, see
[06](06-windows-chkdsk-matrix.md)) rejects is still caught.

---

## The mutation surface

```
   CREATE ────┐                              ┌──── allocate MFT record
   MKDIR  ────┤                              ├──── update parent $INDEX (B-tree)
   UNLINK ────┤   each operation must keep   ├──── update $MFT:$Bitmap
   RMDIR  ────┤   ALL of these consistent:   ├──── update cluster $Bitmap
   RENAME ────┤                              ├──── update $FILE_NAME / link count
   WRITE  ────┤                              ├──── update $DATA runs + sizes
   TRUNCATE ──┤                              ├──── update $STANDARD_INFORMATION
   GROW   ────┘                              └──── leave the volume chkdsk-clean
```

A filesystem write is never one edit — it is a coordinated set of edits across
several structures that must *all* land or the volume is corrupt. The tests are
organized to prove each operation keeps every one of those structures
consistent.

---

## Coverage map: operation → tests

| Write operation | What is verified | Test files |
|---|---|---|
| **Create file** | MFT record allocated; `$STANDARD_INFORMATION` + `$FILE_NAME` + `$DATA` written; parent index updated | `write_create.rs`, `capi_create.rs` |
| **Make directory** | Index allocation; parent links; `.`/`..` semantics | `write_mkdir.rs` |
| **Unlink file** | Record + clusters deallocated; bitmaps freed; hard-link count decremented correctly | `write_unlink.rs`, `capi_remove.rs`, `hardlink_scenarios.rs` |
| **Remove directory** | Empty-only constraint enforced; non-empty rmdir rejected | `write_rmdir.rs`, `capi_remove.rs` |
| **Rename** | Same-directory and cross-directory; same-length and variable-length names; refuse rename onto an existing name | `write_rename.rs`, `write_rename_varlen.rs`, `capi_rename.rs` |
| **Write content** | Resident in-MFT write; in-place non-resident write; correct sizes | `write_content.rs`, `write_resident_contents.rs`, `capi_write_content.rs`, `capi_write_variants.rs` |
| **Grow** | File extension allocates clusters; the grown tail is zero-filled (no stale data leaks in) | `write_grow.rs` |
| **Truncate** | Shrink discards tail clusters and frees them; truncate-then-grow zero-fills | `write_truncate.rs`, `capi_write_truncate.rs` |
| **Resident → non-resident promotion** | Small file in-MFT auto-promotes to allocated clusters when it overflows; exact round-trip | `write_promote.rs`, `resident_threshold.rs`, `boundary_sizes.rs` |
| **Hard links** | Link creation; link-count tracking; deleting one name leaves the others and the data intact | `write_link.rs`, `capi_link.rs`, `hardlink_scenarios.rs` |
| **Alternate Data Streams (write)** | Create/mutate named `$DATA`; resident→non-resident promotion of a stream | `write_ads.rs`, `write_ads_promote.rs`, `capi_ads.rs` |
| **Extended Attributes (write)** | Name/value/flag round-trip; upsert replaces; remove; 4-byte alignment preserved | `write_ea.rs`, `ea_combinatorics.rs`, `capi_ea.rs` |
| **Reparse points (write)** | Symlink and junction writes; tag + target round-trip | `write_reparse.rs`, `capi_reparse.rs` |
| **Timestamps** | Each of the four timestamps settable independently; unset fields preserved | `write_times.rs` |
| **File-attribute flags** | READONLY and other DOS-attribute bits toggle and persist | `write_attrs.rs` |
| **Object ID (write)** | 16-byte and 64-byte (birth-GUID) forms written and read back | `object_id.rs`, `set_object_id_extended_h.rs` |
| **Volume label (write)** | `set_volume_label` round-trips through `$VOLUME_NAME` | `volume_label.rs` |
| **Scale / stress** | Many files (MFT pressure), large files (data-run pagination), large directories | `manyfiles.rs`, `large_file.rs`, `large_directory.rs` |

---

## The three properties every write test guards

### 1. Round-trip fidelity — "what we wrote is what is there"

The dominant pattern: perform the write with `fs-ntfs`, then read it back — often
with the independent `ntfs 0.4` parser — and assert byte-for-byte equality.

```
   write_promote.rs::promote_makes_data_nonresident_with_exact_roundtrip
   ───────────────────────────────────────────────────────────────────
   1. create a small file (resident, lives inside the MFT record)
   2. write 8192 bytes  → forces promotion to allocated clusters
   3. read back with the INDEPENDENT ntfs 0.4 parser
   4. assert: non-resident  AND  bytes match exactly
```

### 2. No data leakage on grow — "the tail is clean"

When a file grows, the newly exposed range must be zeros — never whatever
happened to be in those clusters before. `write_grow.rs` and the truncate-grow
tests explicitly verify the zero-fill invariant on the grown tail. (This is a
classic source of cross-file information disclosure; it is tested directly.)

### 3. Refusal where refusal is correct — "fail safe, not silent"

A safe filesystem says *no* to illegal operations rather than corrupting itself.
`error_paths.rs` (21 tests) is the guardrail suite. Every one of these must
return an error, not corrupt the volume and not panic:

```
   create_file_duplicate_name_errors        create_file_in_missing_parent_errors
   create_file_empty_basename_errors         create_file_dot_and_dotdot_error
   create_file_name_too_long_errors          unlink_missing_file_errors
   unlink_directory_errors                    rmdir_non_empty_errors
```

This includes the *honest-limit* cases (see
[08](08-coverage-and-honest-limits.md)): where a feature is not yet implemented
— e.g. writing to a directory large enough to overflow its in-MFT index — the
driver returns an error rather than attempting a write it cannot do safely.

---

## End-to-end workflows — where state bugs hide

Single-operation tests can each pass while a *sequence* of operations corrupts
state (a freed cluster reused wrongly, a stale size left behind). `end_to_end_workflows.rs`
(10 tests) and `all_images_rw_smoke.rs` (7 tests) chain operations the way a real
user would:

```mermaid
flowchart LR
    A[create file] --> B[resident write]
    B --> C[grow → promote<br/>to non-resident]
    C --> D[truncate]
    D --> E[rename]
    E --> F[read back &<br/>verify every step]
    style F fill:#0b3d0b,color:#fff
```

`single_file_full_lifecycle` walks a file through create → resident write →
promotion → truncate → rename and verifies the result at the end. These compound
tests are the cheap local stand-in for the expensive Windows matrix: they catch
state-corruption bugs in seconds before the change ever costs VM time.

---

## And then it goes to Windows

Passing all of the above earns a change the right to be tried against the real
thing. The write operations here are exactly what the
[chkdsk matrix](06-windows-chkdsk-matrix.md) exercises on a live Windows VM:
format with our code, then let *Windows itself* write/rename/delete on the volume,
then ask `chkdsk` whether it is still clean. The local write tests make that
expensive check almost always pass on the first try.

---

**Next:** [04 — On-disk format & field exhaustion →](04-on-disk-format.md)
