[← Prev: MFT & records](02-mft-records.md) | [TOC](../ntfs-specification.md) | [Next: Indexes & directories →](04-indexes-directories.md)

# 3. Data runs & cluster allocation

This section covers the on-disk representation that maps a non-resident
attribute's logical bytes to physical clusters (the *data run* / mapping-pair
list) and the volume-wide allocator that tracks which clusters are in use
(`$Bitmap`, MFT record 6). It also covers the bookkeeping file for permanently
unusable clusters (`$BadClus`, MFT record 8).

Attribute headers themselves are defined in
[§2 MFT & records](02-mft-records.md). LZNT1-compressed runs are forward-linked
to [§6 Special streams](06-special-streams.md). Crash-consistent ordering of
bitmap writes is detailed in [§5 $LogFile & journal](05-logfile-journal.md).

## Overview {#overview}

A non-resident attribute does not store its value bytes inside the MFT record.
Instead, the record carries a compact list of `(length, lcn)` pairs called
*data runs* (also "mapping pairs"). Each run names a contiguous span of
clusters: how many clusters, and where on disk they live.
[`[OBSERVED: src/data_runs.rs]`](#references).

`$Bitmap` (MFT record 6) is a system file whose unnamed `$DATA` attribute is a
packed bit-per-cluster array covering the whole volume — bit `k` is `1` iff
cluster `k` is allocated. It is the authoritative on-disk allocator state.
[`[OBSERVED: src/bitmap.rs:1-13]`](#references).

The two structures are coupled:

- A non-resident attribute's data runs name a set of LCNs.
- For every non-sparse run, those LCNs **must** be marked allocated in
  `$Bitmap`. If they aren't, the volume is in one of two failure modes:
  *cluster leak* (bitmap says USED, no attribute references it) or
  *cross-link* (attribute references a cluster bitmap says FREE, or two
  attributes reference the same cluster). Both are detected by the chkdsk-style
  Double-Pass reconciliation [UNVERIFIED].
- Conversely, the bitmap is the only authoritative source for "is this cluster
  in use?" — there is no per-cluster back-pointer to the owning attribute on
  NTFS. Reverse mapping requires an MFT walk
  [UNVERIFIED].

The remainder of this section walks the encoding bottom-up: byte format, then
the run-list as a whole, then the bitmap, then the bad-cluster file.

## Data run encoding {#data-run-encoding}

A run-list is a stream of variable-length records terminated by a single
`0x00` byte. Each non-terminator record encodes one run.

**Header byte.** The first byte of every run is a packed nibble pair
[`[OBSERVED: src/data_runs.rs:42-47]`](#references):

```
bit  7 6 5 4   3 2 1 0
     │ │ │ │   │ │ │ │
     │ │ │ │   └─┴─┴─┴── low nibble:  length-of-length-field, in bytes (F)
     └─┴─┴─┴──────────── high nibble: length-of-offset-field, in bytes (V)
```

- `F` (low nibble) = number of bytes that follow holding the run length
  (cluster count).
- `V` (high nibble) = number of bytes that follow holding the LCN offset.
- `F = 0` is invalid (a run must have a non-zero length).
  [`[OBSERVED: src/data_runs.rs:48-50]`](#references).
- `V = 0` is the sparse-run sentinel — see
  [Sparse runs](#sparse-runs) below.
- `F`, `V` are each in `0..=8`. Our implementation accepts `length_len`
  up to 8 (signed/unsigned interpretation tracked in
  `src/data_runs.rs:51-53`); whether that exceeds NTFS itself's tolerance
  is [UNVERIFIED]
  [`[OBSERVED: src/data_runs.rs:51-53]`](#references).

**Length field.** `F` little-endian bytes giving the run's cluster count
as an unsigned integer [`[OBSERVED: src/data_runs.rs:91-95]`](#references). The length is
**always positive** — encoders MUST use enough bytes that the value, when
interpreted by a sign-aware reader, would still be non-negative. In practice
that means: if the high bit of the most significant length byte would be set,
emit one extra zero byte. See
[Encoder length-byte selection](#run-invariants) for the failure mode this
prevents [`[OBSERVED: src/data_runs.rs:144-154]`](#references).

**Offset field.** `V` little-endian bytes giving the LCN as a *signed* delta
from the previous run's LCN
[`[OBSERVED: src/data_runs.rs:75-93]`](#references):

- For the **first** run in a list, the previous-LCN is implicitly `0`, so
  the field reads as the absolute LCN of the run's first cluster.
- For every subsequent run, the field is a signed delta added to the running
  absolute LCN. Negative deltas (the run lives at a lower LCN than the
  previous run) are legal and must be handled with sign extension on the
  most significant byte [`[OBSERVED: src/data_runs.rs:42-57]`](#references).
- Sign extension is performed by setting bits `[V*8 .. 64)` to all-ones if
  the high bit of byte `V-1` is set
  [`[OBSERVED: src/data_runs.rs:80-84]`](#references).

**Terminator.** A single `0x00` byte ends the list. Because `F = 0` is
invalid for a real run, the terminator is unambiguous
[`[OBSERVED: src/data_runs.rs:42-44]`](#references). If the attribute's
`length` field bounds the mapping-pairs region before a `0x00` is seen, the
parser stops at `length` — the terminator is recommended but the bounding
length takes precedence [`[OBSERVED: src/data_runs.rs:103-105]`](#references)
[UNVERIFIED].

The total bytes consumed by one run record is therefore
`1 + F + V` (or just `1` for the terminator).

## Data run examples {#data-run-examples}

### Simple contiguous run

```
Bytes:   31 20 4A 00 05
         │  │        │
         │  │        └── LCN offset: 0x05004A (signed, 3 bytes) = cluster 327754
         │  └─────────── Run length: 0x20 (1 byte) = 32 clusters
         └────────────── Header nibble: 0x31 → offset_len=3, length_len=1
```

Decoded: 32 clusters starting at LCN 327754. As the first run, the offset
field reads as the absolute LCN [`[OBSERVED: src/data_runs.rs:66]`](#references).

### Sparse run

```
Bytes:   01 10
         │  │
         │  └── Run length: 0x10 = 16 clusters
         └───── Header nibble: 0x01 → offset_len=0 (SPARSE), length_len=1
         Note: No LCN bytes follow. This run contributes NO clusters to $Bitmap.
```

Decoded: 16 sparse clusters. The VCN counter advances by 16; the running LCN
counter does **not** change
[`[OBSERVED: src/data_runs.rs:72-74]`](#references). Reads from these VCNs
return zero; no physical storage is occupied.

### Negative LCN delta (backward jump)

```
Bytes:   21 10 00 FE
         │  │  │
         │  │  └── LCN delta: 0xFE00 (signed 16-bit) = -512 clusters from previous LCN
         │  └───── Run length: 0x10 = 16 clusters
         └──────── Header nibble: 0x21 → offset_len=2, length_len=1
```

Decoded: 16 clusters starting 512 clusters below the previous run's LCN.
This appears in fragmented files where the allocator has reused freed
clusters earlier on the disk [UNVERIFIED].

### Multi-run sequence (annotated)

A three-run list combining the patterns above might look like:

```
31 20 4A 00 05    ; run 0: 32 clusters @ LCN 327754
21 10 00 FE       ; run 1: 16 clusters @ LCN 327754 + (-512) = 327242
01 08             ; run 2: 8 sparse clusters
00                ; terminator
```

VCN coverage:
- Run 0: VCN `[0, 32)`
- Run 1: VCN `[32, 48)`
- Run 2: VCN `[48, 56)`

Total length: 56 clusters; this must equal the
`(last_vcn − first_vcn + 1)` recorded in the non-resident attribute header.
[`[OBSERVED: src/attr_io.rs:142-148]`](#references).

## Sparse runs {#sparse-runs}

A sparse run is encoded with `V = 0` (high nibble of the header byte is zero):
no LCN bytes follow, only the length [`[OBSERVED: src/data_runs.rs:101-102]`](#references).
Reads return zero; no cluster is allocated. The decoder represents this in
[`DataRun`](../../../src/data_runs.rs) as `lcn: None`
[`[OBSERVED: src/data_runs.rs:26-29]`](#references).

**Three things sparse runs must NOT do:**

1. **Contribute bits to `$Bitmap`.** A sparse run has no physical LCN, so the
   reconciliation pass must skip it. If the on-disk `$Bitmap` happens to mark
   LCNs that *fall within the same numeric range as* a sparse VCN range as
   USED, those bits are leaks (see
   [Bitmap reconciliation](#bitmap-reconciliation))
   [UNVERIFIED].
2. **Advance the running LCN cursor.** The encoder/decoder treat the
   previous-LCN value as unchanged across a sparse run. Subsequent runs'
   deltas are still relative to the last *physical* run's LCN
   [`[OBSERVED: src/data_runs.rs:72-94]`](#references).
3. **Appear without the attribute being flagged sparse.** An attribute
   carrying a sparse run MUST have the `0x0001` (Sparse) flag set in its
   attribute-header flags field; a sparse run inside a non-sparse
   attribute is a fatal structural contradiction
   (`ERR_SPARSE_FLAG_MISMATCH`)
   [UNVERIFIED].

**Interaction with size fields.** The non-resident attribute header carries
three size fields (offsets per [§2 MFT records](02-mft-records.md)):

- `allocated_size` — total bytes spanned by all runs (sparse + physical),
  rounded up to the cluster size.
- `data_size` — the logical end-of-file in bytes (may be less than
  `allocated_size`).
- `valid_data_length` (a.k.a. `initialized_length`) — bytes from offset 0
  that contain valid data; bytes in `[valid_data_length, data_size)` read as
  zero regardless of what is on disk.

Sparse runs let `allocated_size` exceed the sum of physically allocated
clusters: the file is logically large but consumes no clusters in the
sparse regions. `valid_data_length` is *orthogonal* — it tracks the
post-extend, pre-write zero tail and applies to physical runs too
[`[OBSERVED: src/attr_io.rs:142-148]`](#references) [UNVERIFIED].

## VCN-to-LCN mapping {#vcn-lcn-mapping}

The decoder produces a list of [`DataRun`](../../../src/data_runs.rs) entries,
each carrying its `starting_vcn`, `length`, and (optional) absolute `lcn`.
Translating a virtual cluster number to a logical cluster number is then a
linear (or binary) search:

```
function vcn_to_lcn(runs, vcn):
    for r in runs:
        if r.starting_vcn <= vcn < r.starting_vcn + r.length:
            if r.lcn is None:                 # sparse hole
                return SPARSE
            return r.lcn + (vcn - r.starting_vcn)
    return PAST_END
```

[`[OBSERVED: src/data_runs.rs:111-121]`](#references). The current
implementation does a linear scan, which is fine for typical fragmentation
counts but should be revisited for heavily fragmented files; a cached
`(start_vcn, lcn, count)` triple layout would permit `O(log n)` binary
search [UNVERIFIED]
[`[OBSERVED: src/data_runs.rs:111-121]`](#references).

**Reading VCN k from a non-resident attribute** (the algorithm cluster-cache
hits and misses both reduce to):

1. Locate the attribute in the MFT record (see
   [§2 attribute iteration](02-mft-records.md#attribute-iteration)).
2. Read the mapping-pairs blob at
   `attr_offset + non_resident_mapping_pairs_offset` through
   `attr_offset + attr_length` [`[OBSERVED: src/attr_io.rs:74-76]`](#references).
3. Decode runs.
4. Resolve `vcn_to_lcn(runs, k)`.
5. Read cluster `lcn * cluster_size .. (lcn+1) * cluster_size` from disk;
   if the result is `SPARSE`, return zeros without doing I/O
   [`[OBSERVED: src/data_runs.rs:111-121]`](#references).

## Run list invariants {#run-invariants}

The following invariants hold for every well-formed run list. A parser MUST
treat any violation as corruption.
[UNVERIFIED].

1. **Header `0x00` terminates correctly.** A standalone `0x00` byte ends
   the list; no further bytes are consumed
   [`[OBSERVED: src/data_runs.rs:42-44]`](#references).
2. **`F = 0` rejected.** The length-field-byte-count must be at least 1; a
   run with no length is malformed
   [`[OBSERVED: src/data_runs.rs:48-50]`](#references).
3. **`F > 8` and `V > 8` rejected** as malformed by the parser
   [`[OBSERVED: src/data_runs.rs:51-53]`](#references); whether NTFS itself
   tolerates `F` in `5..=8` is [UNVERIFIED].
4. **Run length is non-zero** after decoding
   [`[OBSERVED: src/data_runs.rs:67-70]`](#references).
5. **No overlap.** Two physical runs in the same attribute MUST NOT
   reference overlapping LCNs. (Cross-attribute overlap = cross-link;
   intra-attribute overlap = self-collision, which is fatal.)
   [UNVERIFIED for intra-attribute].
6. **Monotonically increasing VCN.** Each run's `starting_vcn` equals the
   previous run's `starting_vcn + length`. Gaps in VCN coverage are
   forbidden; a sparse region is encoded as an explicit `V=0` run, not as
   a missing run
   [`[OBSERVED: src/data_runs.rs:101-101]`](#references),
   [`[OBSERVED: src/data_runs.rs:133-139]`](#references).
7. **VCN total matches the non-resident header.** Sum of all runs' lengths
   must equal `last_vcn - first_vcn + 1` from the attribute's non-resident
   header [UNVERIFIED].
8. **Byte total covers `allocated_size`.** Sum of run lengths × cluster
   size MUST be ≥ the attribute's `allocated_size`. Falling short = the
   sequence is truncated [UNVERIFIED].
9. **LCN bounds.** Every absolute LCN computed from deltas must satisfy
   `0 ≤ lcn < total_clusters` and `lcn + length ≤ total_clusters`
   [`[OBSERVED: src/data_runs.rs:89-91]`](#references).

### Encoder length-byte selection {#encoder-length-bytes}

The mapping-pair *length* is unsigned by spec but is encoded with a
byte-count that, by Microsoft format-tool convention, must keep the high bit
of the most significant byte clear (i.e. the bytes-needed function treats
the value as signed when picking width). Emitting too few bytes — for
example, 2 bytes for `0x8000 = 32768` — produces an on-disk encoding that a
sign-aware reader interprets as `−32768`, corrupting the run length.

This was observed against `$BadClus` on a 128 MiB / 4 KiB-cluster volume
where the named `$Bad` sparse run covered 32768 clusters: encoding the
length as `00 80` (2 bytes) made chkdsk report
*"MFT contains a corrupted file record"* (Event ID 55). The fix is to emit
3 bytes (`00 00 80`) so the high bit of the most significant byte stays clear.
[`[OBSERVED: src/data_runs.rs:144-154]`](#references),
[`[OBSERVED: write-smoke-20260502-175747]`](#references).

The encoder uses `signed_bytes_needed(n)`: smallest `N ∈ 1..=8` such that
`-2^(8N-1) ≤ n < 2^(8N-1)`
[`[OBSERVED: src/data_runs.rs:183-195]`](#references).

## Resident → non-resident migration {#resident-migration}

A small attribute's value lives inline in the MFT record (resident). Once it
exceeds a size threshold tied to the available record space, it must be
*made non-resident*: the value is written out to freshly allocated clusters,
the resident header is rewritten as a non-resident header, and the
mapping-pairs blob describing those clusters is appended to the attribute
header [`[OBSERVED: src/mkfs.rs:760-774]`](#references) [UNVERIFIED].

**Threshold semantics.** An attribute is forced non-resident when its value
plus the fixed non-resident header (≈ 64 bytes) plus the mapping-pairs
length would no longer fit in the MFT record's free space — i.e. the
threshold is **dynamic**, depending on what other attributes already occupy
the record. There is no fixed "value > N bytes ⇒ non-resident" rule
[UNVERIFIED].

**Write-driven migration.** Normal write-driven migration is performed by
the writer whenever a resident attribute outgrows its record. The current
`rust-fs-ntfs` writer does not yet implement migration in either direction;
it errors when a resident attribute would need to grow past its record space
[`[OBSERVED: docs/STATUS.md:436-448]`](#references) [UNVERIFIED].

**Reverse migration (non-resident → resident).** Some operating-system
implementations are documented to migrate a non-resident attribute back to
resident on truncate-to-zero. Behavior is implementation-defined; the
on-disk format permits either choice
[`[NTFSCOM: NTFS Attributes]`](#references) [UNVERIFIED].

## $Bitmap (record 6) {#bitmap-overview}

`$Bitmap` is the system file at MFT record number 6
[`[OBSERVED: src/bitmap.rs:27]`](#references). It carries one
unnamed `$DATA` attribute whose value is a packed bit array, one bit per
cluster on the volume.

**Bit-to-cluster mapping** [`[OBSERVED: src/bitmap.rs:1-7]`](#references):

```
cluster k is allocated  ⇔  byte (k / 8), bit (k % 8) is set to 1
```

The bit position within a byte is **LSB-first**: cluster 0 is bit 0 of byte 0
(mask `0x01`), cluster 7 is bit 7 of byte 0 (mask `0x80`), cluster 8 is bit 0
of byte 1, and so on
[`[OBSERVED: src/bitmap.rs:230-245]`](#references) — i.e. the same bit
order as the `BIT_FIELD_REF` macros in MS-FSCC for cluster bitmaps
[`[MS-FSCC §2.6]`](#references) [UNVERIFIED].

**Residence.** On any volume large enough to matter, `$Bitmap` is non-resident:
the unnamed `$DATA` attribute carries a mapping-pairs list whose runs name
the clusters that hold the bitmap itself. The bitmap clusters are themselves
marked allocated in the bitmap (bootstrap is fine — mkfs.rs writes the
bitmap and sets the bits in a single pass)
[`[OBSERVED: src/bitmap.rs:39-68]`](#references),
[`[OBSERVED: src/mkfs.rs:368-407]`](#references).

**Sizing.** The bitmap has exactly `total_clusters` valid bits. The
attribute's non-resident `value_length` (bytes) covers
`ceil(total_clusters / 8)` bytes; padding to a cluster boundary is
zero-filled [`[OBSERVED: src/bitmap.rs:60-67]`](#references). Bits in the
final byte beyond `total_clusters` are reserved and must be set to `1` so
the allocator never picks them
[`[OBSERVED: src/mkfs.rs:401-405]`](#references).

**Reading and writing bits.** Locate the run containing the target VCN
(`byte_offset / cluster_size`), translate to LCN, read or write the byte at
`(lcn + (vcn - run.starting_vcn)) * cluster_size + byte_offset_in_cluster`,
then mask in the bit. The `mutate_bits_io` helper does this with a
read-modify-write across the affected byte range
[`[OBSERVED: src/bitmap.rs:209-249]`](#references).

**`$MFT:$BITMAP` is a different bitmap.** The `$MFT` system file (record 0)
also carries a `$BITMAP` attribute, but that one is a record-allocation
bitmap (one bit per MFT record, not per cluster). It is reconciled with the
same Double-Pass algorithm but capped to the actual MFT record count to
avoid off-by-N errors [UNVERIFIED].

## $Bitmap update rules {#bitmap-update-rules}

The contract for cluster allocation is:

1. **Find a free run** of the requested length (`find_free_run` does a
   first-fit linear scan with a starting hint, wrapping at the end of the
   bitmap) [`[OBSERVED: src/bitmap.rs:115-175]`](#references).
2. **Set bits** in `$Bitmap` for `[lcn, lcn + n)`. A double-allocation —
   any bit in the range is already 1 — is a hard error and aborts the
   operation [`[OBSERVED: src/bitmap.rs:235-237]`](#references).
3. **Write data** to the cluster range.
4. **Update the attribute's mapping-pairs and size fields** to point at
   the newly allocated run.

Free is the inverse: clear bits, then update the attribute. Double-free
(any bit in the range already 0) is a hard error
[`[OBSERVED: src/bitmap.rs:238-240]`](#references).

**Ordering vs. crash safety.** The current implementation issues a `sync`
after each bitmap write but does not yet integrate with `$LogFile` — see
[§5 $LogFile & journal](05-logfile-journal.md) for the journaled write
path that must wrap these updates
[`[OBSERVED: src/bitmap.rs:323-324]`](#references) [UNVERIFIED].

**Allocate-before-write vs. write-before-allocate.** For new attribute data,
the safe order is: allocate (set bitmap bits) → write attribute data → update
attribute mapping-pairs. If a crash occurs between bitmap-set and
mapping-pairs-update, the result is a leaked cluster (recoverable by
reconciliation). The reverse order risks a cross-link, which is
unrecoverable without data loss [UNVERIFIED].

For free, the order is reversed: update mapping-pairs first (so no attribute
references the cluster), then clear the bit. A crash mid-flow leaks the
cluster again — never cross-links
[UNVERIFIED].

## $Bitmap reconciliation {#bitmap-reconciliation}

The Double-Pass algorithm builds a *Ground Truth* bitmap from an MFT walk
and reconciles it against the on-disk `$Bitmap`
[UNVERIFIED].

### Pass 1 — Ground-Truth construction

Allocate a zero-filled bitmap of `total_clusters` bits. Walk every MFT
record. For each record where `IN_USE` is set:

- For every non-resident attribute on the record, decode its data runs.
- For each non-sparse run, set the bits `[run.lcn, run.lcn + run.length)`
  in the ground-truth buffer.
- Sparse runs contribute nothing.
- A corrupt run does NOT abort Pass 1; it logs a warning and skips, so the
  rest of the volume's truth is mappable [UNVERIFIED].
- If a cluster is already set when about to be set again — that is a
  *cross-link*. Survival priority is fixed:
  System (MFT 0–15) > User > Younger files (by `$UsnJrnl` or
  `LastModificationTime`). The losing file is truncated or marked corrupt
  [UNVERIFIED].

### Pass 2 — Bit-by-bit reconciliation

For every cluster, compare ground-truth (`computed`) to disk (`on_disk`):

| `computed` | `on_disk` | Diagnosis           | Action                                |
| ---------- | --------- | ------------------- | ------------------------------------- |
| FREE       | FREE      | OK                  | nothing                               |
| USED       | USED      | OK                  | nothing                               |
| FREE       | USED      | Cluster leak        | clear bit on disk (reclaim)           |
| USED       | FREE      | Cross-link / risk   | set bit on disk (enforce)             |

[UNVERIFIED].

The "cross-link / risk" case is the dangerous one — it means an attribute
references a cluster the bitmap thinks is free, so the allocator could hand
that cluster out and overwrite live data. The reconciliation enforces the
attribute's claim by marking USED [UNVERIFIED].

### Safety barriers

Before committing a *bulk free* (e.g., > 5% of the volume), a
heuristic-consensus barrier aborts the free if the ground-truth
construction looks unreliable [UNVERIFIED]:

- *EIO threshold*: < 0.1% I/O errors during Pass 1.
- *MFT yield*: > 90% valid `FILE` records vs. the `$MFT:$BITMAP` count.
- *Lost-cluster ratio*: ≤ 10% USED-on-disk-but-FREE-in-truth.

If ≥ 2 of 3 fail, the engine throws `ERR_CONSENSUS_FAILED` and refuses to
free [UNVERIFIED]. The full reconciliation must also be
preceded by a WAL snapshot — see
[§5 $LogFile & journal](05-logfile-journal.md).

### TRIM/DISCARD prohibition

When clearing leaked-cluster bits, the engine MUST NOT issue
TRIM/DISCARD to the underlying device. A TRIM physically erases the flash
cells; if the leak diagnosis is wrong (parser bug, ground-truth construction
error), the data is unrecoverable. TRIM is left for the OS to issue post-mount
once the volume is known consistent [UNVERIFIED].

`rust-fs-ntfs` is a userspace library and does not call
`ioctl(BLKDISCARD)` or its equivalents
[`[OBSERVED: src/bitmap.rs]`](#references).

## $Bad (record 8) {#bad-clusters}

`$BadClus` is the system file at MFT record number 8
[`[OBSERVED: src/mkfs.rs:64,729]`](#references).
Its job is to permanently quarantine clusters that have failed I/O so the
allocator never hands them out again.

**Layout.** `$BadClus` carries two `$DATA` attributes
[`[OBSERVED: src/mkfs.rs:729-774]`](#references):

1. **Unnamed `$DATA`** — empty, resident, zero bytes. Present so the file's
   default stream exists.
2. **Named `$DATA` with name `"$Bad"`** (UTF-16LE, name length = 4). This
   is the one that carries the bad-cluster bookkeeping.

**Identifying the named attribute** [`[OBSERVED: src/mkfs.rs:47,229,905-907]`](#references):

- Type code = `0x80` (`$DATA`).
- Name length = 4.
- Name (UTF-16LE) exactly equals `"$Bad"`.

A correct match on all three is required before treating the data runs as
bad-cluster ranges.

**Run encoding.** The `$Bad` attribute's data runs cover the *entire data
portion of the volume* — every cluster except the last (which holds the
backup boot sector and is excluded by NTFS convention) — encoded as a
single sparse run on a freshly formatted volume
[`[OBSERVED: src/mkfs.rs:735-749]`](#references). Each cluster known to be
bad is then turned from sparse into a physical run pointing at *itself*
(LCN = own LCN), which has the effect of "this cluster is named in the
bad-cluster file, and it is allocated to the bad-cluster file"
[UNVERIFIED — own-LCN convention].

The size fields tell a coordinated story: the `$Bad` attribute reports
`allocated_size = (cluster_count − 1) × cluster_size` and
`data_size = (cluster_count − 1) × cluster_size`, while the file's
`$FILE_NAME` tracks the unnamed `$DATA` (which is empty) — both `0`
[`[OBSERVED: src/mkfs.rs:760-774]`](#references). Microsoft `format.com`
matches this shape; an off-by-one was previously observed where the
`$Bad` length covered `cluster_count` instead of `cluster_count − 1`,
which caused chkdsk /scan to flag the volume as needing offline /F
[`[OBSERVED: src/mkfs.rs:738-742]`](#references),
[`[OBSERVED: run-20260503-072644]`](#references).

**Consistency with `$Bitmap`** [UNVERIFIED]:

- Every cluster named in `$Bad`'s physical (non-sparse) runs MUST be
  marked USED in `$Bitmap`. If a named bad cluster is FREE in `$Bitmap`,
  the bitmap is wrong — fix it (set USED).
- A cluster USED in `$Bitmap` but NOT in `$Bad` is fine — it may be
  legitimately allocated to a regular file. No action.

The asymmetry is deliberate: `$Bad` is a quarantine list, not an
allocation list, so `$Bad ⊆ allocated`.

## Bad sector relocation flow {#bad-sector-relocation}

When a hardware bad sector is detected (an `EIO` on read of a cluster
belonging to a critical file), the engine MUST relocate the affected
data and quarantine the failing LCN [UNVERIFIED].

### Trigger

A read of a cluster belonging to `$MFT`, `$Bitmap`, `$MFTMirr`, `$LogFile`,
or any user file returns a hard I/O error
[UNVERIFIED]. Soft errors (transient, retried-and-succeeded)
do **not** trigger relocation.

### Mark & Reallocate

1. **Append the failing LCN to `$BadClus:$Bad`.** The sparse run covering
   that LCN is split: the LCN becomes a physical run referencing itself
   (or the bookkeeping equivalent), with neighbors remaining sparse
   [UNVERIFIED — split mechanics].
2. **Set the bit for that LCN in `$Bitmap`** to USED, so the allocator
   never hands it out [UNVERIFIED].
3. **Allocate a healthy replacement cluster** via `find_free_run` against
   `$Bitmap` [`[OBSERVED: src/bitmap.rs:115-175]`](#references).
4. **Update the affected file's data runs** to point at the new LCN
   instead of the bad one [UNVERIFIED].
5. **Reconstruct the lost data** in the new cluster — the source depends
   on which file was hit (see "Per-file reconstruction" below).

### Per-file reconstruction

| File hit       | Reconstruction source                                              |
| -------------- | ------------------------------------------------------------------ |
| `$MFT` (rec ≤ N) | Copy from `$MFTMirr` if record is within the mirrored range. Otherwise mark FREE; orphan recovery handles it [UNVERIFIED]. |
| `$MFT` (rec > N) | Mark record FREE; trigger orphan recovery [UNVERIFIED]. |
| `$MFTMirr`     | Relocate `$MFTMirr` and update its LCN in **both** the primary and backup boot sectors. Failing that, log `WARN_MFTMIRR_REDUNDANCY_LOST` and proceed without redundancy [UNVERIFIED]. |
| `$Bitmap`      | Re-run the Double-Pass Ground-Truth (Pass 1) and write the result into the new cluster [UNVERIFIED]. |
| User file      | Cluster contents are lost (no source); zero-fill or truncate the affected VCN range, mark the file in some "data lost" state [UNVERIFIED — exact policy]. |

### Bootstrap problem: `$BadClus` itself on a bad sector

If the cluster(s) holding `$BadClus`'s own data runs are unreadable, the
relocation logic cannot use `$BadClus` to record the failure (circular
dependency). The recovery is to **rebuild `$BadClus` from scratch** by
collecting every LCN that triggered an EIO during the Pass 1 MFT scan and
the Pass 5 record validation, then reconstructing
`$BadClus:$Bad`'s data runs from that list. Cross-check that all
collected bad LCNs are USED in `$Bitmap`
[UNVERIFIED].

### Boundary conditions

- **No healthy clusters available.** If `find_free_run` returns `None`,
  the relocation cannot proceed; the engine falls back to logging a
  fatal warning and continuing without redundancy where applicable
  [UNVERIFIED].
- **`$Bitmap` itself is the relocated file.** The replacement cluster is
  named in the new `$Bitmap`'s mapping-pairs list, which is itself written
  into the new bitmap clusters — a chicken-and-egg situation handled by
  finalizing the bitmap *after* its own runs are decided
  [UNVERIFIED].
- **Both boot sectors must update.** Relocating `$MFTMirr` requires
  updating both the primary boot sector at LBA 0 and the backup at the
  end of the volume; missing the backup leaves a stale pointer that can
  resurrect the bad LCN on the next mount
  [UNVERIFIED].

`rust-fs-ntfs` does not currently implement bad-sector relocation; the
specification is captured here for the future repair-mode implementation
[`[OBSERVED: docs/STATUS.md]`](#references) [UNVERIFIED].

## References

- `[MS-FSCC §2.6]` — MS-FSCC bit-field cluster-bitmap conventions
  (referenced for byte/bit ordering corroboration).
- `[NTFSCOM: NTFS Attributes]` — ntfs.com attribute documentation
  (general background on attribute residency).
- `[OBSERVED: src/data_runs.rs]` — encoder/decoder implementation.
- `[OBSERVED: src/bitmap.rs]` — `$Bitmap` allocator implementation.
- `[OBSERVED: src/attr_io.rs]` — attribute walker (mapping-pairs offset
  resolution).
- `[OBSERVED: src/mkfs.rs]` — initial layout, `$BadClus` shape,
  `$Bitmap` initialization.
- `[OBSERVED: write-smoke-20260502-175747]` — `$Bad` length-byte
  sign-extension bug repro.
- `[OBSERVED: run-20260503-072644]` — `$Bad` cluster-count-vs-cluster-count-1
  off-by-one repro vs. Microsoft `format.com` reference.
- `[OBSERVED: docs/STATUS.md]` — implementation status of W2.3 (`$Bitmap`
  allocator), W2.4 (encoder), and unimplemented repair-mode features.

## Open questions

Section-local `[UNVERIFIED]` items. See also
[`docs/spec/notes/open-questions.md`](../notes/open-questions.md).

- 🔬 No per-cluster back-pointer to owning attribute on NTFS — confirm by
  byte-level inspection of MS-formatted volumes and by absence of any
  reverse-map structure in `[MS-NTFS]`.
- 🔬 External pseudocode caps `length_len > 4`; implementation accepts up
  to 8. Confirm the actual ntfs.sys upper bound by writing a probe
  attribute with a 5-byte length field and observing chkdsk behavior.
- 🔬 Negative LCN delta example (`21 10 00 FE`) is uncorroborated; produce
  a black-box round-trip with a fragmented file that lays clusters
  backward and confirm the encoded bytes against a Microsoft-formatted
  volume.
- 🔬 Behavior of run-list parsing when the attribute's bounding `length`
  ends mid-run (no `0x00` seen). Current decoder tolerates; confirm
  ntfs.sys behavior.
- 🔬 `ERR_SPARSE_FLAG_MISMATCH` — sparse run inside non-sparse-flagged
  attribute. Construct test image and observe chkdsk verdict.
- 🔬 VCN-to-LCN linear scan vs. an alternative binary-search caching
  strategy. Measure fragmented-file read perf to decide whether to
  switch.
- 🔬 `valid_data_length` semantics for sparse + physical mixed runs.
  Write attribute with mid-file sparse hole, observe Windows-side reads
  in the `[valid_data_length, data_size)` range.
- 🔬 Resident → non-resident migration threshold is dynamic (record
  free-space dependent), not a fixed byte count. Probe by growing a
  resident attribute byte-by-byte and observe at which length the
  Microsoft writer migrates.
- 🔬 Reverse migration on truncate-to-zero — implementation-defined per
  ntfs.com; confirm whether the Microsoft writer ever moves a non-resident
  attribute back to resident.
- 🔬 LSB-first within byte for `$Bitmap` per `[MS-FSCC §2.6]` — confirm
  the section number actually documents this (current cite is best-guess
  pending FSCC re-read).
- 🔬 WAL ordering of `$Bitmap` writes vs. `$LogFile` records — current
  implementation `sync`s after each write but does not journal.
  Cross-link with §5 once journal write path lands.
- 🔬 Allocate-before-write vs. write-before-allocate ordering claim is
  implementation-folklore; confirm against `[MS-NTFS]` write semantics.
- 🔬 Cross-link survival priority (System > User > Younger) is
  uncorroborated; no permitted source confirms the ordering.
- 🔬 `$Bad` "physical run pointing at itself" convention — the encoded
  LCN of a quarantined cluster in `$BadClus:$Bad`. Confirm by dumping
  a Microsoft-formatted volume after a deliberate bad-sector injection.
- 🔬 `$BadClus` sparse-run split mechanics on bad-sector append (turn one
  sparse run into [sparse | physical | sparse]). Behavior unverified.
- 🔬 Per-file reconstruction policy for user files hit by EIO — no
  permitted source specifies the exact zero-fill vs. truncate choice.
- 🔬 `$Bitmap` self-relocation (the file itself sitting on bad clusters)
  has a chicken-and-egg in writing the new mapping-pairs into the new
  bitmap. Verify the sequencing.

[← Prev: MFT & records](02-mft-records.md) | [TOC](../ntfs-specification.md) | [Next: Indexes & directories →](04-indexes-directories.md)
