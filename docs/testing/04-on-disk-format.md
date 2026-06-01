# 04 — On-Disk Format & Field Exhaustion

> *Filesystem corruption almost never comes from the obvious case. It comes from
> the boundary — the 0-byte file, the field that is exactly at its maximum, the
> value one byte over the limit. This layer is built to live at those boundaries.*

This is the foundation layer: the 525 unit tests in `src/` plus the
structural and field-exhaustion integration tests. They work at the level of
individual bytes and individual on-disk structures — below the public API, where
a single wrong shift or off-by-one silently corrupts a volume.

---

## The structures under test

```
        ┌──────────────────────────────────────────────────────────┐
        │                    NTFS on-disk format                    │
        ├──────────────┬───────────────┬───────────────────────────┤
        │  $Boot       │  $MFT records │  allocation                │
        │  (BPB)       │  + fixups     │  ($Bitmap, $MFT:$Bitmap)   │
        ├──────────────┼───────────────┼───────────────────────────┤
        │  data runs   │  attributes   │  directory index           │
        │  (mapping    │  (resident /  │  ($INDEX_ROOT,             │
        │   pairs)     │   non-res)    │   $INDEX_ALLOCATION blocks) │
        └──────────────┴───────────────┴───────────────────────────┘
              ▲              ▲                    ▲
        data_runs.rs    attr_io.rs          index_io.rs
        (+36 unit)      (+24 unit)          (+46 unit)
        mft_io.rs       attr_resize.rs      idx_block.rs
        (+34 unit)      (+13 unit)          (+17 unit)
        bitmap.rs       record_build.rs     mft_bitmap.rs
        (+31 unit)      (+60 unit)          (+23 unit)
```

Unit-test counts per module (from `grep -c '#\[test\]' src/*.rs`):

| Module | Unit tests | Responsibility |
|---|---:|---|
| `write.rs` | 69 | the write engine itself |
| `record_build.rs` | 60 | assembling MFT records byte-by-byte |
| `index_io.rs` | 46 | directory B-tree index read/write |
| `lib.rs` | 43 | top-level API + glue |
| `data_runs.rs` | 36 | mapping-pair (VCN↔LCN) encode/decode |
| `mft_io.rs` | 34 | MFT record read-modify-write + fixups |
| `bitmap.rs` | 31 | cluster allocation bitmap |
| `ea_io.rs` | 27 | extended-attribute codec |
| `block_io.rs` | 24 | block device access |
| `attr_io.rs` | 24 | attribute iteration/parsing |
| `mkfs.rs` | 23 | the formatter |
| `mft_bitmap.rs` | 23 | MFT-record allocation bitmap |
| `fsck.rs` | 22 | recovery / dirty-flag / `$LogFile` |
| `idx_block.rs` | 17 | index allocation blocks |
| `sds.rs` | 16 | security descriptor stream |
| `upcase.rs` | 15 | case-folding table |
| `attr_resize.rs` | 13 | growing/shrinking attributes |
| `fs_core_bridge.rs` | 2 | block-device bridge |

That is **525 unit tests** concentrated exactly where a byte-level mistake would
corrupt a volume.

---

## Data runs: the most dangerous codec

NTFS stores where a file's clusters live as *mapping pairs* — a variable-length
encoding (1–3 bytes per field) of signed deltas. It is compact and it is
treacherous: an off-by-one in the length nibble, or mishandling a negative delta,
silently points a file at the wrong clusters. `data_runs.rs` (20 integration +
36 unit tests) covers:

```
   empty terminator            single contiguous run
   two runs, positive delta    run with negative delta (LCN decreases)
   sparse run (hole)           mixed sparse + dense
   VCN→LCN walk across runs    encode → decode round-trip
```

The encode/decode round-trip is the key invariant: anything we encode, we must be
able to decode back to the same runs — and this same codec is one of the three
[fuzz targets](05-robustness-and-fuzzing.md), so it is also hammered with
millions of hostile inputs.

---

## MFT records & the fixup dance

Every MFT record carries an *Update Sequence Array* (fixup) — the last two bytes
of each 512-byte sector are replaced with a sequence number and the originals are
stashed in an array, so torn writes are detectable. Getting this wrong corrupts
the record. `mft_io.rs` (9 integration + 34 unit tests) covers fixup
apply/remove round-trips, USN bumps, and the sector-end slots where the fixup
lives.

---

## Allocation bitmaps: never lose or double-allocate a cluster

Two bitmaps must stay perfectly consistent with reality, or you get cross-linked
files (catastrophic) or leaked space:

- the **cluster `$Bitmap`** — which clusters are in use;
- the **`$MFT:$Bitmap`** — which MFT records are in use.

`bitmap.rs` (10 integration + 31 unit) and `mft_bitmap.rs` (23 unit) verify
locate / allocate / free / find-free-run, and — critically — **double-free
detection**, the bug that produces cross-linked files where two files claim the
same cluster.

---

## Field exhaustion: the boundary discipline

This is the project's signature technique for corruption resistance. For each
on-disk field, the tests sweep the value space to its limits and one step past:

```
        value range of a field
   ┌──────────────────────────────────────────────┐
   │  0  │  1  │  ...typical...  │  MAX  │  MAX+1   │
   └──┬──┴──┬──┴────────┬────────┴───┬───┴────┬─────┘
      │     │           │            │        │
   must   must       must         must     must be
   round  round      round        round    REJECTED
   trip   trip       trip         trip     (not corrupt)
```

Five field-exhaustion suites, **66 tests**, cover the metadata structures most
likely to harbor an off-by-one:

| Suite | Tests | Fields swept |
|---|---:|---|
| `field_exhaustion_ea.rs` | 16 | EA name 1→254 bytes (255 rejected), value 0→hundreds of bytes incl. full 0x00–0xFF binary, NEED_EA flag, 4-byte alignment, upsert/remove |
| `field_exhaustion_si.rs` | 16 | All four `$STANDARD_INFORMATION` timestamps independently, DOS attr bits, v3.x trailer (owner/security-id/quota/USN) |
| `field_exhaustion_fn.rs` | 11 | `$FILE_NAME` sizes, timestamps, DOS name, namespace flags, parent ref — parsed from raw on-disk bytes per MS-FSCC §2.4.4 |
| `field_exhaustion_reparse.rs` | 12 | Reparse tag, version, data size, symlink-target length boundaries |
| `field_exhaustion_objid.rs` | 11 | 16-byte GUID + birth-volume / birth-object / domain IDs |

Representative cases from `field_exhaustion_ea.rs`:

```
   single_char_name_roundtrips           name_max_254_bytes_roundtrips
   long_name_roundtrips (200 bytes)      name_over_254_bytes_rejected     ← MAX+1
   zero_length_value_roundtrips          single_byte_value_roundtrips
   binary_value_with_all_byte_values_roundtrips   ← 0x00..=0xFF, no encoding assumptions
   need_ea_flag_preserved                upsert_same_name_replaces_value
   remove_ea_then_others_remain          remove_nonexistent_ea_errors
```

The `binary_value_with_all_byte_values_roundtrips` test is a good example of the
mindset: it stores a value containing *every* byte from `0x00` to `0xFF` to prove
the codec makes no assumption about printable text, NUL-termination, or encoding.

---

## Boundary sizes: the resident/non-resident cliff

Small files live *inside* the MFT record (resident); once they exceed a threshold
they must spill out to allocated clusters (non-resident). The exact threshold
depends on how much room the record's other attributes leave — so
`boundary_sizes.rs` (13 tests) *empirically binary-searches* the ceiling for a
given filename, then tests:

```
   empty_file_data_is_resident_zero_length      (0 bytes)
   one_byte_file_is_resident                    (1 byte)
   exactly_at_resident_ceiling_stays_resident   (== ceiling)
   one_over_ceiling_rejected_by_resident_path   (ceiling + 1)   ← the cliff edge
   exactly_one_cluster_is_nonresident           (4096 bytes)
   multi_cluster_nonresident_spans_clusters     (allocated ≠ logical size)
   filename_length_1 / _255 (max) / _256 (rejected)
```

`resident_threshold.rs` (8 tests) sweeps sizes across the same boundary, and
`cluster_size_matrix.rs` (10 tests) repeats key cases across **512 B, 1 K, 2 K,
4 K, 8 K, and 64 K** cluster sizes — because a shift that works at 4 K can
overflow at 64 K (a real bug class: `sectors_per_cluster = 0x80` must be read as
a literal, not a log2 exponent).

---

## Why this layer earns trust

- It is the **always-green gate**: all 525 unit tests run with `cargo test --lib`
  in well under a second, with zero setup, on any platform.
- It targets the exact places corruption originates — codecs, bitmaps, fixups,
  boundaries — not just "happy path" reads and writes.
- Its round-trip invariants feed directly into the fuzzers
  ([05](05-robustness-and-fuzzing.md)) and its outputs are the volumes handed to
  `chkdsk` ([06](06-windows-chkdsk-matrix.md)).

---

**Next:** [05 — Robustness, corruption & fuzzing →](05-robustness-and-fuzzing.md)
