# 02 — The Read Path

> *Before you can safely write a filesystem, you have to read it perfectly.
> Every byte we later write is validated by reading it back — so the read path is
> the foundation the entire safety argument stands on.*

The read path answers: *given an existing NTFS volume, can we faithfully report
what is on it?* — mount, `stat`, list a directory, read file contents, follow a
symlink, enumerate alternate data streams and extended attributes, decode object
IDs and volume metadata.

These tests run against pre-built NTFS disk-image fixtures and parse them with the
upstream `ntfs = "0.4"` read-only parser as an independent oracle (see
[01](01-strategy-and-the-contract.md) on why independence matters).

---

## What an NTFS read touches

```
  Boot sector ($Boot)                  ← cluster size, MFT location
        │
        ▼
  $MFT  ── record ── record ── record   ← one record per file/dir
                       │
                       ├─ $STANDARD_INFORMATION   timestamps, DOS attrs, security id
                       ├─ $FILE_NAME              name(s), namespace, parent ref
                       ├─ $DATA (unnamed)         the file's content
                       ├─ $DATA:stream (named)    Alternate Data Streams
                       ├─ $INDEX_ROOT / $INDEX_ALLOCATION   directory B-tree
                       ├─ $EA / $EA_INFORMATION   Extended Attributes
                       ├─ $REPARSE_POINT          symlinks, junctions
                       └─ $OBJECT_ID              16-byte GUID identity
```

Every box above has tests pointed directly at it. Here is the map.

---

## Coverage map: NTFS structure → tests

| NTFS structure / operation | What is verified | Test files |
|---|---|---|
| **Boot sector + mount** | Cluster size, sector size, MFT location parsed; volume opens | `facade.rs`, `mkfs_roundtrip.rs` |
| **`stat` / attribute enumeration** | Every attribute on a record is listed with correct type code, name, offset, length; resident *and* non-resident | `read_attributes.rs`, `read_si_full.rs` |
| **`$STANDARD_INFORMATION`** | All MS-FSCC §2.4.2 fields: 4 timestamps, DOS attribute bits, and the v3.x trailer (owner_id, security_id, quota, USN) | `read_si_full.rs`, `security_id.rs` |
| **`$FILE_NAME`** | Name decoding, namespace flags (POSIX/Win32/DOS), case preservation, multi-name (8.3 + long) files | `read_file_names.rs`, `long_names.rs` |
| **`readdir` (`$INDEX_ROOT` + `$INDEX_ALLOCATION`)** | Directory entries enumerated; `.`/`..` handled; deep nesting traversed | `facade.rs`, `readdir_dots.rs`, `deep.rs`, `path_dots.rs` |
| **`read` file content** | Resident (in-MFT), non-resident, and fragmented `$DATA` all return exact bytes | `facade.rs`, `integration.rs` |
| **Sparse files** | Holes (unallocated ranges) read back as zeros without consuming storage | `sparse.rs` |
| **Alternate Data Streams (named `$DATA`)** | Streams enumerated and read independently of the main stream | `list_named_streams.rs`, `ads.rs`, `ads_combinatorics.rs`, `ads_comprehensive.rs` |
| **Extended Attributes (`$EA`)** | EA names listed cheaply; values decoded; multiple EAs per file | `list_ea_keys.rs`, `ea_combinatorics.rs` |
| **Reparse points (`readlink`)** | Symlink and junction targets extracted and classified | `readlink.rs`, `symlink_variants.rs` |
| **`$OBJECT_ID`** | 16-byte GUID and the 64-byte extended form (birth volume / birth object / domain IDs) | `object_id.rs`, `set_object_id_extended_h.rs` |
| **Volume metadata** | Label (UTF-8 decode), version/flags, dirty bit, capacity & free space | `volume_label.rs`, `volume_info_v2.rs`, `volume_stats.rs` |
| **Unicode & collation** | UTF-16 names with BMP + supplementary chars; `$UpCase` normalization; case-insensitive ordinal comparison | `unicode.rs`, `upcase.rs`, `compare_names_ordinal.rs` |

---

## Why the read path is trustworthy

**1. The hard cases are tested, not just the happy path.** A naive reader handles
a small resident file in the easy case. These tests deliberately exercise:

- *Fragmented content* — files whose `$DATA` is split across non-contiguous
  clusters (multiple data runs), so the cluster-walk logic is exercised, not just
  a single contiguous extent.
- *Sparse content* — files with holes, where the reader must synthesize zeros for
  ranges that occupy no storage (`sparse.rs`).
- *Multi-name files* — files carrying both a long Win32 name and an 8.3 DOS name,
  where naive code double-counts directory entries (`long_names.rs`,
  `read_file_names.rs`).
- *Full Unicode* — names using the full UTF-16 range and the `$UpCase` table for
  case folding (`unicode.rs`, `upcase.rs`).

**2. The public façade is tested as a unit.** `facade.rs` (12 tests) drives the
high-level Rust API exactly as an embedder would: mount, `stat`, `readdir`,
`read_file`, `volume_info`, `is_dirty`. If the ergonomic API is wrong, these fail
even when the low-level parsing is right.

**3. Reads are the oracle for writes.** Every write test in
[03](03-write-path.md) ends by reading the result back — frequently with the
*independent* `ntfs 0.4` parser. So the read path is not only tested directly; it
is exercised thousands more times as the verification half of every write test.

---

## A representative slice

From `data_runs.rs` and the read-content tests, the granularity looks like this
(function names are illustrative of the level of detail):

```
  decode_single_run_contiguous          ← one extent, the easy case
  decode_two_runs_with_positive_delta   ← fragmented forward
  decode_run_with_negative_delta        ← fragmented backward (LCN goes down)
  decode_sparse_run                     ← a hole
  decode_mixed_sparse_and_dense         ← holes interleaved with data
  vcn_to_lcn_walks_multiple_runs        ← virtual→physical cluster mapping
```

That is the texture of the whole read suite: not "can it read a file" but "can it
read *every shape* of file the on-disk format permits."

---

**Next:** [03 — The write path →](03-write-path.md)
