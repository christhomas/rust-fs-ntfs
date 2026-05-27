[← Prev: Data runs & cluster allocation](03-data-runs-bitmap.md) | [TOC](../ntfs-specification.md) | [Next: $LogFile & journal →](05-logfile-journal.md)

# 4. Indexes & directories

## Overview {#overview}

In NTFS, every directory and several system metadata stores are realised as a
**B+ tree over a sorted key space** rather than as a linear list. The tree is
addressed by name and lives entirely inside ordinary MFT attributes:

- The root of the tree is always **resident** inside an `$INDEX_ROOT` (type
  `0x90`) attribute on the owning MFT record.
- When the tree outgrows what can fit resident, additional fixed-size **INDX
  blocks** are stored in a non-resident `$INDEX_ALLOCATION` (type `0xA0`)
  attribute, and a `$BITMAP` (type `0xB0`) attribute tracks which of those
  blocks are in use.
- All three attributes share a *named stream*. For directory entries the
  conventional name is `$I30`; security, quota, object-ID, reparse, and USN
  view indexes use other reserved names (see [§4.13](#other-indexes)).

The canonical example is the directory index `$I30`. Its keys are
`$FILE_NAME` (type `0x30`) attributes, sorted under the
`COLLATION_FILE_NAME` rule (case-insensitive UTF-16 via the volume's
`$UpCase` table — see [§6 Special streams](06-special-streams.md)).
[OBSERVED: STATUS.md "directory indexes"]

The on-disk shape implied by these attributes is a B+ tree:

- **Internal nodes** carry sorted keys and per-key child pointers (VCNs into
  `$INDEX_ALLOCATION`).
- **Leaf nodes** carry sorted keys plus the value (e.g. the full
  `$FILE_NAME` for a directory entry).
- A **terminating "last" sentinel entry** closes every node — leaves and
  internals alike. The sentinel has `key_length = 0` and the `LAST` flag set;
  on internal nodes it still carries a child VCN as its trailing 8 bytes,
  giving the right-most descent.

`rust-fs-ntfs` implements the read path for `$I30` end-to-end and the write
path for the resident-only case (root-level insertion / removal /
same-length rename). Multi-level B+-tree balancing on `$INDEX_ALLOCATION`
is partially implemented for traversal but not for insertion; large
directories are accepted on read, but writes refuse to operate on them
until B+-tree split/merge lands. ✅ read / 🟡 write
[OBSERVED: STATUS.md `$INDEX_ALLOCATION` walks; chkdsk-improvement-findings.md §2.7 root $I30 population]

The rest of this section walks the layout in order, then covers the
named-index variants, collation rules, and the read-side awareness needed
to recognise orphaned INDX content.

## $FILE_NAME (0x30) attribute {#file-name-attr}

The `$FILE_NAME` attribute is both the **filename storage** on a file's MFT
record (one per hard link) and the **key value** stored inside `$I30` index
entries. Its structure is identical in both contexts. It is always
resident.
[OBSERVED: src/index_io.rs, src/record_build.rs]

### Layout {#file-name-layout}

Offsets are relative to the start of the attribute *value* (i.e. after the
24-byte resident attribute header).

| Off  | Size | Field                          | Notes                                                          |
| ---- | ---- | ------------------------------ | -------------------------------------------------------------- |
| 0x00 | 8    | `parent_directory_reference`   | MFT reference: 48-bit record number + 16-bit sequence          |
| 0x08 | 8    | `creation_time`                | Windows NT FILETIME (100 ns since 1601-01-01)                  |
| 0x10 | 8    | `modification_time`            | NT FILETIME                                                    |
| 0x18 | 8    | `mft_change_time`              | NT FILETIME (a.k.a. "C-time")                                  |
| 0x20 | 8    | `access_time`                  | NT FILETIME                                                    |
| 0x28 | 8    | `allocated_size`               | Bytes of `$DATA` allocation (clusters × cluster\_size)         |
| 0x30 | 8    | `real_size`                    | Logical bytes of `$DATA`                                       |
| 0x38 | 4    | `file_attributes`              | DOS-style attribute bitmask (read-only / hidden / dir / …)     |
| 0x3C | 4    | `ea_size_or_reparse_tag`       | Union: `EaSize` for files w/ EA, `ReparseTag` if reparse       |
| 0x40 | 1    | `name_length`                  | UTF-16 code units in `name` (max 255)                          |
| 0x41 | 1    | `namespace`                    | 0=POSIX, 1=Win32, 2=DOS, 3=Win32&DOS                           |
| 0x42 | 2·N  | `name`                         | UTF-16LE code units (no terminator)                            |

[OBSERVED: src/record_build.rs#L666-L711]

The four timestamps mirror the four timestamps in `$STANDARD_INFORMATION`
on the *same* MFT record at the moment the link was created or last
renamed; Windows does **not** keep the `$FILE_NAME` timestamps in sync
with subsequent `$STANDARD_INFORMATION` updates — that mismatch is normal
and the MS-NTFS spec calls it out as expected behaviour.
[OBSERVED: STATUS.md "Per-field nullable; only updates SI, not the parent index"]
[UNVERIFIED]

`allocated_size` and `real_size` are similarly *snapshot* values written at
hardlink time and at rename, then maintained loosely; chkdsk does not flag
stale sizes here as corruption.
[OBSERVED: docs/STATUS.md `set_times` semantics] [UNVERIFIED]

### Multiple $FILE_NAME attributes per record {#multiple-fns}

A single MFT record carries one `$FILE_NAME` attribute **per hard link**,
plus one extra `$FILE_NAME` per separate-namespace alias. The common
configurations are:

- **Single Win32&DOS (`namespace=3`)** — when the long name already
  satisfies 8.3 constraints. One attribute, both namespaces.
- **Win32 (1) + DOS (2) pair** — the long name violates 8.3; a separate
  short alias is generated.
- **POSIX (0)** — legacy / case-sensitive name; modern Windows keeps the
  flag for compatibility but folds POSIX into Win32 collation in repair.
  [UNVERIFIED]

Hardlinks add a fresh `$FILE_NAME` per link with its own
`parent_directory_reference`. Each hardlink corresponds 1:1 with a live
entry in some directory's `$I30`; the MFT `LinkCount` is the cardinality
of those entries (excluding the `Win32&DOS` collapse — see
[§4.4](#dos83-generation)). [UNVERIFIED]

The `name_length` byte is in **UTF-16 code units**, not bytes — a name
with one supplementary-plane character (`\u{1F600}`) consumes 2 code units
(a surrogate pair) but only 1 grapheme.
[OBSERVED: src/index_io.rs `name_length` is `u8` units]

## Namespace types {#namespaces}

The 1-byte `namespace` field at offset `0x41` of the `$FILE_NAME` value
takes one of four values. The semantic distinctions matter for both the
DOS 8.3 alias machinery ([§4.4](#dos83-generation)) and for collation
([§4.11](#collation-rules)).

| Value | Constant     | Meaning                                                     |
| ----- | ------------ | ----------------------------------------------------------- |
| 0     | `POSIX`      | Case-sensitive. Any Unicode except `NUL` and `/`            |
| 1     | `Win32`      | Case-preserving, case-insensitive (the LFN namespace)       |
| 2     | `DOS`        | Strict 8.3, uppercase-only, restricted character set        |
| 3     | `Win32&DOS`  | Long name already satisfies 8.3; one attr serves both       |

[MS-FSCC §2.1.5.2 file-name namespace] [OBSERVED: src/mkfs.rs NAMESPACE_POSIX=0, NAMESPACE_WIN32_DOS=3 (lines 137–138); src/record_build.rs FILE_NAME_NAMESPACE_POSIX=0, FILE_NAME_NAMESPACE_WIN32_AND_DOS=3 (lines 94–95)]

Notes:

- **POSIX is largely a relic.** Modern Windows formatters do not emit
  POSIX entries for names that fit 8.3; `rust-fs-ntfs::mkfs` uses POSIX
  for `$Extend` children whose names exceed 8.3 (e.g. `$ObjId`, `$Reparse`,
  `$Quota`) because shipping WIN32_AND_DOS on those names causes chkdsk
  Stage 2 to reject them with "An invalid filename X (N) was found in
  directory M" — per mkfs.rs comment (lines 127–134). The claim that
  POSIX is treated identically to Win32 for collation is not confirmed
  from code alone. [OBSERVED: src/mkfs.rs NAMESPACE_POSIX docstring; partially UNVERIFIED (collation-fold claim)]
- **A directory entry never has multiple namespaces in one entry.** When
  a long name needs a DOS alias, *two* index entries appear in `$I30` —
  one with `namespace=1` (Win32) and one with `namespace=2` (DOS) —
  pointing to the **same** MFT record number.
  [UNVERIFIED]
- **`Win32&DOS` collapses the pair.** When the LFN is already 8.3-legal
  uppercase, Windows writes a single entry with `namespace=3` instead of
  two redundant entries.
- **`rust-fs-ntfs::mkfs` selects the namespace per-name** via
  `fn_namespace_for(name)` (`src/record_build.rs`): names that fit
  the DOS 8.3 envelope (stem ∈ [1, 8] chars + ext ∈ [0, 3] chars + no
  leading/trailing dot + every char in the canonical DOS 8.3 alphabet)
  return `namespace = 3` (Win32&DOS); everything else returns
  `namespace = 0` (POSIX). The rule is symmetric on both sides of the
  index — both the MFT-record `$FILE_NAME` and the embedded copy in
  each `$INDEX_ROOT` / `$INDEX_ALLOCATION` entry MUST carry the same
  namespace value; chkdsk Stage 2 reports
  `Index entry 'X' in index $I30 of file 5 is incorrect` when the two
  copies disagree.
  [`[OBSERVED: src/record_build.rs::fn_namespace_for, docs/mkfs-bug-catalog.md Bug 12 + Bug 13]`](#references)
- **chkdsk asymmetry.** chkdsk validates the namespace byte and rejects
  `WIN32_AND_DOS` (3) on names that do not satisfy the DOS 8.3 envelope
  ("An invalid filename X (N) was found in directory M" — chkdsk Stage 2).
  Whether lone `DOS` entries without a `Win32` counterpart are explicitly
  flagged is not confirmed from code alone. [OBSERVED: src/mkfs.rs lines 79–93 (chkdsk rejection of wrong namespace); partially UNVERIFIED (lone-DOS flagging)]

The `IsCaseSensitive`-on-directory feature added in Windows 10 *(per-dir
case sensitivity flag)* does not introduce a new namespace value; it is
encoded elsewhere as a directory attribute. [UNVERIFIED]

## DOS 8.3 generation algorithm {#dos83-generation}

When the LFN is not already valid 8.3, Windows synthesises a deterministic
short alias. *Windows' exact tier-3 hash is undocumented and implementations
are free to use any deterministic 16-bit hash that yields collision-free
results.* [UNVERIFIED]

### Step 1 — normalise {#dos83-step1}

1. Uppercase the LFN using the volume's own `$UpCase` table (not the host
   OS's Unicode table). [UNVERIFIED]
2. Strip leading and trailing spaces.
3. Strip leading periods.
4. Keep only the *last* period as the base/extension separator; remove
   all earlier periods.
5. Replace illegal 8.3 characters with `_`. The illegal set is:

   ```
   "  *  /  :  <  >  ?  \  |  +  ,  ;  =  [  ]
   ```

   plus any byte with code-point < `0x20`. [UNVERIFIED]

### Step 2 — truncate {#dos83-step2}

- Extension = first 3 valid characters after the last period (empty if
  no period).
- Basename = first 6 valid characters before the last period.
  [UNVERIFIED]

### Step 3 — collision resolution (3-tier) {#dos83-step3}

Each candidate is checked against the parent directory's existing `$I30`
contents — *every* entry, not only previously-generated 8.3 names.

| Tier | Format                  | Range       | Example          |
| ---- | ----------------------- | ----------- | ---------------- |
| 1    | `{base6}~{N}.{ext3}`    | N = 1–9     | `MYLONG~1.TXT`   |
| 2    | `{base5}~{NN}.{ext3}`   | N = 10–99   | `MYLON~10.TXT`   |
| 3    | `{base2}{HHHH}~{N}.{ext3}` | N > 99   | `MY021F~1.TXT`   |

[UNVERIFIED]

`HHHH` is four hex digits from a 16-bit hash of the LFN. The hash function
is **not** mandated — any deterministic 16-bit hash yields a structurally
valid name; only the cosmetic appearance differs from Windows. [UNVERIFIED]

### Volume-level disablement {#dos83-disable}

If `NtfsDisable8dot3NameCreation` is set on the host volume, Windows does
not generate DOS aliases at all. Repair tools must respect this and skip
regeneration. The negative signal is detectable from disk state alone:
the absence of *any* DOS-namespace entries on the volume implies the flag
is set. [UNVERIFIED]

`rust-fs-ntfs` does not currently synthesise paired DOS aliases. Each
basename gets a single index entry whose namespace is chosen by
`fn_namespace_for(name)` (see the namespace bullets above): names
satisfying the DOS 8.3 envelope ship as `namespace=3` (Win32&DOS);
all others ship as `namespace=0` (POSIX). Names that need a strict
`namespace=1` / `namespace=2` pair (LFN + paired DOS short alias)
are not generated — the closest existing functional limit. ⛔
paired-DOS-alias write
[OBSERVED: `src/record_build.rs::fn_namespace_for`, `STATUS.md`]

## $INDEX_ROOT (0x90) {#index-root}

`$INDEX_ROOT` is a **resident** attribute that holds:

1. A small fixed header naming the type of attribute being indexed and
   the collation rule.
2. An `INDEX_HEADER` describing the entries that follow (always present,
   even when empty).
3. Zero or more index entries, terminated by a `LAST` sentinel.

If the entries fit in the resident `$INDEX_ROOT`, the index is fully
contained here and there is no `$INDEX_ALLOCATION`. If they overflow, the
`HAS_SUBNODES` bit is set in the `INDEX_HEADER` flags and the entries
become *internal-node* entries pointing at INDX leaves in
`$INDEX_ALLOCATION`. [OBSERVED: src/index_io.rs]

### INDEX_ROOT_HEADER (16 bytes) {#index-root-header}

| Off  | Size | Field                       | Notes                                                          |
| ---- | ---- | --------------------------- | -------------------------------------------------------------- |
| 0x00 | 4    | `attribute_type_indexed`    | `0x30` for `$I30` (i.e. `$FILE_NAME` keys); `0x00` for views   |
| 0x04 | 4    | `collation_rule`            | Enum — see [§4.11](#collation-rules)                           |
| 0x08 | 4    | `index_block_size`          | Bytes per INDX block (e.g. 4096)                               |
| 0x0C | 1    | `clusters_per_index_block`  | Signed: positive = clusters, negative = log2 of bytes/block    |
| 0x0D | 3    | padding                     | Zero                                                           |

[OBSERVED: src/record_build.rs#L201-L216]

For a directory `$I30`, `attribute_type_indexed = 0x30` and `collation_rule
= 1` (`COLLATION_FILE_NAME`). For view indexes (`$Secure`, `$Quota`,
`$ObjId`) the indexed-attribute field is `0x00` because the value is
synthetic, not a copy of an MFT attribute. [OBSERVED: src/mkfs.rs
`build_populated_named_index_root_attr` calls for `$ObjId`, `$Reparse`,
`$Quota` all pass `0` as `attribute_type_indexed`; src/record_build.rs
`write_empty_index_root` passes `0x30` for `$I30`]

The `clusters_per_index_block` byte (at INDEX_ROOT body offset `0x0C`)
shares the *spirit* of the boot-sector `clusters_per_mft_record` /
`clusters_per_index_buffer` encoding but uses a **different** rule in
the smaller-than-cluster case:

| Relation                              | Byte value                          |
| ------------------------------------- | ----------------------------------- |
| `cluster_size ≤ index_block_size`     | `index_block_size / cluster_size` (clusters per block — positive) |
| `cluster_size  > index_block_size`    | `index_block_size / 512` (sectors per block — positive)           |

This is **not** symmetric with boot offset `0x44`
(`ClustersPerIndexBuffer`), which uses signed-negative-log2 in the
second branch. For a 4 KiB index block on an 8 KiB cluster volume the
boot byte is `-log2(4096) = -12 = 0xF4`, but the `INDEX_ROOT.cpib`
byte is `4096 / 512 = 8` (sectors-per-block). `chkdsk` Stage 2
reports "Error detected in index $I30 for file 5" when these byte
forms diverge from `format.com`'s reference (corroborated against
512 / 1024 / 4096 / 8192 cluster-size scenarios)
[`[OBSERVED: src/mkfs.rs::build_populated_index_root_attr]`](#references).

Two `rust-fs-ntfs` code paths handle this differently:

- `src/mkfs.rs::build_populated_index_root_attr` (the root `$I30`
  populated at format time) takes `cluster_size` and applies the
  table above [OBSERVED].
- `src/record_build.rs::build_index_root_skeleton_attr` (used for
  fresh `$I30` skeletons during runtime mkdir) hardcodes `1` and
  assumes `index_block_size == cluster_size`. Adequate for matrix
  scenarios which all create directories at the volume's default
  cluster size, but would need the same per-cluster-size encoding
  if non-default block sizes were ever requested
  [`[OBSERVED: src/record_build.rs:300-303]`](#references).

### INDEX_HEADER (16 bytes) {#index-header}

| Off  | Size | Field                  | Notes                                                                   |
| ---- | ---- | ---------------------- | ----------------------------------------------------------------------- |
| 0x00 | 4    | `first_entry_offset`   | Relative to start of `INDEX_HEADER`                                     |
| 0x04 | 4    | `total_size`           | From `INDEX_HEADER` start through the end of entries (incl. sentinel)   |
| 0x08 | 4    | `allocated_size`       | Capacity of the entry area                                              |
| 0x0C | 1    | `flags`                | Bit 0 = `HAS_SUBNODES` (index spills to `$INDEX_ALLOCATION`)            |
| 0x0D | 3    | padding                | Zero                                                                    |

[OBSERVED: src/index_io.rs IH offsets]

**Invariant `allocated_size ≥ total_size`** (updates on insert/delete).
Both fields move together when index entries are inserted or
removed; `allocated_size` may equal `total_size` (no slack) or
exceed it (slack reserved for future inserts in the same node), but
must never be smaller. Writing only one of the two — for example,
bumping `total_size` past `allocated_size` after an `$INDEX_ROOT`
insert — produces a chkdsk Stage 1 Event 55 against `$I30:$INDEX_ROOT`
even when the entry payload itself is correctly placed
[`[OBSERVED: docs/mkfs-bug-catalog.md Bug 11]`](#references).

For an empty directory, the entry area contains just one entry — the
`LAST` sentinel — with `length = 16`, `key_length = 0`, `flags = 0x02`.
`total_size` = 32 (= `INDEX_HEADER` size + sentinel size). [OBSERVED:
src/record_build.rs#L218-L240]

The same `INDEX_HEADER` layout reappears inside every INDX record
([§4.8](#indx-record)); the only difference is the wrapping container.

## $INDEX_ALLOCATION (0xA0) {#index-allocation}

`$INDEX_ALLOCATION` is the **non-resident** attribute that backs the leaf
(and any non-root internal) nodes of the B+ tree. Its value is a stream
of fixed-size **INDX blocks**, each of which is a multi-sector record
with its own `INDX` magic and Update Sequence Array fixup
(see [§2 USA fixup](02-mft-records.md#usa-fixup)).
[OBSERVED: src/idx_block.rs#L93 "non-resident expected"; INDX magic + USA fixup confirmed at src/idx_block.rs#L151-L158]

Properties:

- **Always non-resident.** A directory whose entries fit resident in
  `$INDEX_ROOT` has *no* `$INDEX_ALLOCATION` at all. The presence of
  this attribute and the `HAS_SUBNODES` bit on the `INDEX_HEADER` are
  required to agree.
  [OBSERVED: src/idx_block.rs#L93 "non-resident expected"]
- **Block size is fixed** at format time and recorded in
  `$INDEX_ROOT.index_block_size`. Common values: 4096 bytes (the
  formatter default for cluster sizes ≤ 4096) or one cluster.
  [UNVERIFIED]
- **Cluster-aligned data runs.** The data runs encode VCN→LCN mappings
  in clusters, so blocks smaller than a cluster pack multiple INDX
  records per cluster.
  [OBSERVED: src/idx_block.rs `vcn_to_disk_offset`]
- **`$BITMAP:$I30` co-exists.** Allocation state is held *outside* the
  data stream — see [§4.7](#index-bitmap).

A reader walks `$INDEX_ALLOCATION` by:

1. Decoding the `$INDEX_ROOT.index_block_size` from the parent record.
2. Decoding the `$INDEX_ALLOCATION:$I30` data runs to a VCN list.
3. Decoding `$BITMAP:$I30` to know which VCNs hold *live* INDX blocks
   (bit set) versus free space (bit clear).
4. Reading each live VCN-aligned block, validating the `INDX` magic, and
   applying USA fixup before parsing entries.

[OBSERVED: src/idx_block.rs `load_for_directory_io` + `read_indx_block_io`]

### When $INDEX_ALLOCATION is needed {#when-allocation-needed}

The trigger is that the sorted-entry stream (sentinel included) cannot
fit in the resident `$INDEX_ROOT` while still leaving room for the
record's other attributes inside one MFT record. In practice the NTFS
formatter **always** allocates `$INDEX_ALLOCATION` for the root directory
(record 5) of a freshly-formatted volume, regardless of whether it is
actually populated, because the system files inserted at format time
overflow a single record's resident space. [OBSERVED: STATUS.md
`ntfs-manyfiles.img` test rationale; chkdsk-improvement-findings.md
§2.7 root $I30 overflow]

For ordinary subdirectories, the threshold depends on cluster size,
record size, and the average key length. A typical 1024-byte record with
a 4096-byte index block transitions from resident-only to non-resident
around 8–10 entries with mid-length names. [UNVERIFIED]

## $BITMAP (0xB0) for indexes {#index-bitmap}

The `$BITMAP:$I30` attribute holds one bit per INDX block in the
allocation, in **VCN-block order**. Bit `k` set ⇒ the INDX block whose
start VCN is `k * (block_size / cluster_size)` is live; clear ⇒ the
block is free / unwritten.
[OBSERVED: src/idx_block.rs#L36-L54]

Properties:

- **Resident or non-resident.** Small bitmaps (≤ 256 bits ≈ 256 INDX
  blocks ≈ 1 MiB of allocation at 4096 byte blocks) are typically
  resident; larger ones spill to non-resident clusters. `rust-fs-ntfs`
  currently rejects non-resident `$BITMAP:$I30` as unsupported in the
  read path's MVP. 🟡 [OBSERVED: src/idx_block.rs#L109]
- **Padded to byte boundary.** Trailing bits in the last byte are zero.
  [UNVERIFIED]
- **Authoritative.** A block whose bit is *clear* in `$BITMAP:$I30` is
  not part of any tree, even if its on-disk bytes still parse as a
  valid INDX record. Repair sweeps treat such blocks as free space and
  may reclaim their clusters in `$Bitmap` (the volume cluster bitmap —
  see [§3 cluster allocation](03-data-runs-bitmap.md)).
  [UNVERIFIED]

This is the *index-local* bitmap; it must not be confused with the
volume-wide `$Bitmap` (record 6) covered in §3.

## INDX record layout {#indx-record}

Each block inside `$INDEX_ALLOCATION` is an **INDX record**: a
multi-sector structure of size `index_block_size`, USA-protected just
like MFT records.
[OBSERVED: src/idx_block.rs#L151-L158 validates `INDX` magic + applies USA fixup; block size sourced from `$INDEX_ROOT.index_block_size`]

### Block header (24 bytes) {#indx-block-header}

| Off  | Size | Field                       | Notes                                                          |
| ---- | ---- | --------------------------- | -------------------------------------------------------------- |
| 0x00 | 4    | `magic`                     | `'I' 'N' 'D' 'X'` = `0x58444E49` little-endian                 |
| 0x04 | 2    | `update_sequence_offset`    | Bytes from start of block to USA. Typical: `0x0028`            |
| 0x06 | 2    | `update_sequence_count`     | `1 + (block_size / sector_size)` words                         |
| 0x08 | 8    | `lsn`                       | `$LogFile` Sequence Number — see [§5](05-logfile-journal.md)   |
| 0x10 | 8    | `this_vcn`                  | The VCN of *this* block within `$INDEX_ALLOCATION`             |

Bytes `[0x18, 0x18 + sizeof(INDEX_HEADER))` then hold an
[`INDEX_HEADER`](#index-header) describing the entries that follow.
Entries start at `0x18 + first_entry_offset` and run for `total_size`
bytes including the trailing `LAST` sentinel.
[OBSERVED: src/idx_block.rs USA + IH offsets]

### USA fixup {#indx-usa}

Same protocol as MFT records: the USA stores one *update sequence number*
plus one fixup word per sector. Before reading the entry stream, the last
two bytes of each sector are validated against the USN and replaced with
the saved fixup word. Cross-link [§2 USA fixup](02-mft-records.md#usa-fixup)
for the byte-level procedure.

The signature `INDX` (vs. `FILE` for an MFT record, `RCRD` for a
`$LogFile` page) is a strict precondition for fixup; reading code rejects
a block whose first four bytes are not `INDX`. [OBSERVED: src/idx_block.rs#L151-L158]

### Free-space pointer {#indx-free-space}

The `INDEX_HEADER.allocated_size` field gives the capacity of the entry
area; `INDEX_HEADER.total_size − INDEX_HEADER` size gives the live byte
count. The difference is free space at the tail. There is no explicit
free-list — insertion either appends before the sentinel (if there is
room) or splits the node. [UNVERIFIED]

## Index entry header {#index-entry}

Every index entry — in both `$INDEX_ROOT` and INDX-block contexts — has
the same fixed 16-byte prefix.

| Off  | Size | Field                  | Notes                                                                  |
| ---- | ---- | ---------------------- | ---------------------------------------------------------------------- |
| 0x00 | 8    | `file_reference`       | MFT reference for the indexed object (low 48 bits = record number)     |
| 0x08 | 2    | `length`               | Total bytes in this entry, including key + value + optional VCN tail   |
| 0x0A | 2    | `key_length`           | Bytes in the key; equals stream-value length for a `$FILE_NAME` key    |
| 0x0C | 2    | `flags`                | Bit 0 = `HAS_SUBNODE`, bit 1 = `LAST` (terminating sentinel)           |
| 0x0E | 2    | `padding`              | Reserved / zero                                                        |
| 0x10 | …    | `key`                  | `key_length` bytes; for `$I30`, a `$FILE_NAME` value                   |
| …    | …    | `value`                | Optional, present only for view indexes (e.g. `$Q`, `$O`)              |
| `length-8` | 8 | `subnode_vcn`        | Only if `flags & HAS_SUBNODE`; child VCN in `$INDEX_ALLOCATION`        |

[OBSERVED: src/index_io.rs IE_* offsets]

Notes:

- For a directory `$I30`, the *value* is empty: the `$FILE_NAME` is the
  key and the `file_reference` already locates the target. Entry length
  is therefore `0x10 + key_length` (rounded up to 8) plus `8` if
  `HAS_SUBNODE`. [OBSERVED: src/record_build.rs sentinel = 16 bytes]
- For view indexes (`$Q`, `$SII`, `$O`, …) the value follows the key and
  carries the index payload (e.g. quota record, security ID, object ID
  payload).
- The **`LAST` sentinel** has `key_length = 0`. On a leaf node it has
  `flags = 0x02`; on an internal node it has `flags = 0x03` and an
  8-byte `subnode_vcn` tail giving the right-most child.
  [OBSERVED: src/index_io.rs IE_FLAG_LAST handling]
- **Alignment.** Each entry is 8-byte aligned; the writer pads the entry
  body to 8 before counting `length`. [OBSERVED: src/record_build.rs
  `align8` applied to all entry sizing; `build_index_entry` in src/mkfs.rs
  pads key bytes to 8-byte boundary before writing `entry_length`]
- **`file_reference` for sentinels** is conventionally zero in the
  NTFS formatter's output; readers must not interpret a sentinel's
  reference as pointing to MFT record 0.
  [OBSERVED: src/record_build.rs sentinel `file_reference = 0`]

### File-reference encoding {#file-reference-encoding}

The 8-byte `file_reference` is the standard NTFS MFT reference:

| Bits  | Meaning                                       |
| ----- | --------------------------------------------- |
| 0–47  | MFT record number                             |
| 48–63 | Sequence number (incremented when record is freed and reused) |

Index-walk readers must mask off the high 16 bits before resolving the
record. The sequence is checked against the target record's own sequence
field and a mismatch indicates a stale entry that must be ignored.
[OBSERVED: src/record_build.rs `encode_file_reference` (low 48 = record number, high 16 = sequence); sequence checking on lookup is not directly confirmed in code — [UNVERIFIED] for the stale-entry detection behaviour]

## B-tree node layout {#btree-node}

A node — whether the resident root inside `$INDEX_ROOT` or an INDX block
in `$INDEX_ALLOCATION` — is a sorted run of index entries terminated by
a `LAST` sentinel. The B+ tree property holds:

- **Entries are sorted by key under the indexed attribute's collation
  rule.** [OBSERVED: src/mkfs.rs line 1339 `sys_entries.sort_by(|a, b| collate_file_name(a.1, b.1))` before emitting root `$I30`; `collate_file_name` maps each name through ASCII-upcase UTF-16 comparison, matching `COLLATION_FILE_NAME`]
- **Internal nodes carry per-entry child pointers.** Every entry in an
  internal node — including the `LAST` sentinel — has the
  `HAS_SUBNODE` flag set, and the last 8 bytes of the entry are the
  child VCN.
- **Leaf nodes carry no child pointers.** Their `LAST` sentinel has
  flags `= 0x02` exactly.
- **Right-most descent uses the sentinel's child.** When a search key
  is greater than every key in the node, the walker descends through
  the sentinel's `subnode_vcn`.
- **The root flag `HAS_SUBNODES`** on the `INDEX_HEADER` indicates
  whether *any* entry in this node has children — equivalently,
  whether this is a non-leaf root.
  [OBSERVED: src/record_build.rs `write_empty_index_root` sets `flags = 0` (no subnodes) for a fresh empty directory; the `HAS_SUBNODES` bit path is described in src/index_io.rs]

A binary search inside a node is permitted under any of the collation
rules below and is the typical implementation; a linear scan is also
correct for small nodes. `rust-fs-ntfs`'s read path uses linear scan
inside a node. [OBSERVED: src/index_io.rs `find_index_entry` walk]

## Collation rules {#collation-rules}

The 4-byte `collation_rule` field in `INDEX_ROOT_HEADER` selects the
total ordering used for keys. The specific values below apply in the
context of `$I30` and `$Q`: [OBSERVED: src/mkfs.rs constants at lines 50–65 + emission sites; see per-row sources below]

| Value | Constant                  | Used by                            | Comparison                                                                |
| ----- | ------------------------- | ---------------------------------- | ------------------------------------------------------------------------- |
| 0x00  | `COLLATION_BINARY`        | (not observed in codebase)         | Byte-wise unsigned; spec-listed but no code in this repo emits it         |
| 0x01  | `COLLATION_FILE_NAME`     | `$I30` (directory entries)         | Case-insensitive UTF-16 via the volume's `$UpCase` table                  |
| 0x02  | `COLLATION_UNICODE_STRING`| Generic UTF-16 view indexes        | Code-unit comparison, case-sensitive UTF-16                               |
| 0x10  | `COLLATION_NTOFS_ULONG`   | `$Quota\$Q`, `$Secure\$SII`        | Little-endian `u32` numeric                                               |
| 0x11  | `COLLATION_NTOFS_SID`     | `$Quota\$O`                        | NT SID structural ordering                                                |
| 0x12  | `COLLATION_NTOFS_SECURITY_HASH` | `$Secure\$SDH`               | (security hash, security ID) lexicographic                                |
| 0x13  | `COLLATION_NTOFS_ULONGS`  | `$Reparse\$R`, `$ObjId\$O`         | Sequence of little-endian `u32` lexicographic                             |

Sources, by row:

- `0x01` `COLLATION_FILE_NAME` — [OBSERVED: src/mkfs.rs:50, src/record_build.rs#L205 `collation_rule = 1`].
- `0x10` `COLLATION_NTOFS_ULONG` — [OBSERVED: src/mkfs.rs:59 — comment cites MS-FSCC §2.4; emitted for `$Secure:$SII` (mkfs.rs:1148) and `$Quota:$Q`].
- `0x11` `COLLATION_NTOFS_SID` — [OBSERVED: src/mkfs.rs:65 — comment cites MS-FSCC §2.4; emitted for `$Quota:$O` (mkfs.rs:1304)].
- `0x12` `COLLATION_NTOFS_SECURITY_HASH` — [OBSERVED: src/mkfs.rs:55 — comment cites MS-FSCC §2.4; emitted for `$Secure:$SDH` (mkfs.rs:1133)].
- `0x13` `COLLATION_NTOFS_ULONGS` — [OBSERVED: src/mkfs.rs:63 — comment cites MS-FSCC §2.4; emitted for `$ObjId:$O` (mkfs.rs:1306) and `$Reparse:$R` (mkfs.rs:1329)].
- `0x00` `COLLATION_BINARY` — [UNVERIFIED] — not emitted by any code in this codebase; `$ObjId:$O` uses `COLLATION_NTOFS_ULONGS` (0x13) per `src/mkfs.rs`.
- `0x02` `COLLATION_UNICODE_STRING` — [UNVERIFIED].

### COLLATION_FILE_NAME details {#collation-file-name}

The **only** collation rule used by directory `$I30` indexes. The
algorithm:

1. Take the Win32-namespace UTF-16 form of each name (POSIX is folded to
   Win32 for repair). [UNVERIFIED]
2. Map each UTF-16 code unit through the volume's `$UpCase` table to get
   its uppercase form. [UNVERIFIED]
3. Compare the resulting code-unit sequences lexicographically. Shorter
   names sort before longer names with the same prefix.

Critical points:

- **Use the volume's own `$UpCase`, not the host OS's Unicode tables.**
  The Unicode revision used at format time may differ from the host's;
  any name whose case mapping disagrees becomes "invisible" to Windows
  in the wrong order. [UNVERIFIED]
- **Case folding, not full Unicode normalisation.** No NFC/NFD step;
  pre-composed and decomposed forms of the same name compare different.
  [UNVERIFIED]
- **Surrogate pairs compare in code-unit order.** This places
  supplementary-plane characters after `0xD7FF` but before `0xE000`
  surrogates and before `0xE000`+ BMP characters — a known
  documentation gotcha but consistent with code-unit lexicographic
  order. [UNVERIFIED]

Cross-link to [§6 $UpCase](06-special-streams.md) for the contents of
the `$UpCase` table.

## Directory index $I30 {#i30-directory}

The directory index `$I30` is the canonical use of NTFS indexes. Every
directory MFT record carries:

- **`$INDEX_ROOT:$I30`** with `attribute_type_indexed = 0x30`
  (`$FILE_NAME`) and `collation_rule = 0x01` (`COLLATION_FILE_NAME`).
- **`$INDEX_ALLOCATION:$I30`** if the entry stream overflows the
  resident root.
- **`$BITMAP:$I30`** alongside `$INDEX_ALLOCATION`, tracking live INDX
  blocks.

[OBSERVED: src/idx_block.rs
`load_for_directory_io`]

The MFT record's flags reflect the directory status:

- `MFT_RECORD_IS_DIRECTORY = 0x0002` — set whenever `$INDEX_ROOT:$I30`
  is present. [OBSERVED: docs/chkdsk-improvement-findings.md §2.4 flag
  table]
- `MFT_RECORD_IS_VIEW_INDEX = 0x0008` — **not** set for `$I30`; this
  flag is reserved for *non-`$FILE_NAME`* view indexes (`$Secure`,
  `$Quota`, `$ObjId`, `$Reparse`).
  [OBSERVED: docs/chkdsk-improvement-findings.md §2.2.3]

### Coexistence with named $DATA streams {#i30-and-data}

Directories may carry named `$DATA` streams (e.g. NTFS alternate data
streams written by browsers as `Zone.Identifier`, or
`com.apple.quarantine` written by macOS). The presence of `$INDEX_ROOT`
on a record does **not** preclude a `$DATA` attribute on the same
record; index-rebuild logic must not assume mutual exclusion.
[UNVERIFIED]

### Dot/dotdot entries {#i30-dotdot}

NTFS does **not** materialise `.` and `..` index entries inside `$I30`.
The parent reference is recovered from each child's
`$FILE_NAME.parent_directory_reference` field instead, and a directory's
own self-reference is its MFT record number. Layers above NTFS (e.g. the
Win32 `FindFirstFile` API, FUSE bridges) synthesise `.`/`..` on the fly.
[OBSERVED: src/mkfs.rs root `$I30` builder emits no `.` or `..` key entries — the loop at lines 1346–1374 only emits system-record `$FILE_NAME` streams, none named `..`; `rust-fs-ntfs` synthesises them above the raw index layer]

`rust-fs-ntfs` synthesises these in its directory-listing API; they are
never stored on disk. [OBSERVED: STATUS.md "directory listing"]

### Root-directory $I30: required entries and sort order {#i30-root-required}

A fresh-format volume's root directory `$I30` MUST list **all 12
system files** (slots 0..=11) plus the root's self-link `.`, sorted
by `COLLATION_FILE_NAME`. Each entry's embedded `$FILE_NAME` is a
**skeleton**: only `parent_reference` and `name` are populated;
every other field (`creation_time`, `modification_time`,
`allocated_size`, `data_size`, `file_attributes`, `reparse_point`,
etc.) is zero. The only fully-populated entry is the `$MFT`
self-reference at index 0.

The 11 system-file names, in `COLLATION_FILE_NAME` order
(case-insensitive ordinal via the canonical `$UpCase` table, with
the `$` prefix sorting at its ASCII codepoint `0x24`):

| Order | Record | Name        |
| ----: | -----: | ----------- |
| 0     | 5      | `.`         |
| 1     | 4      | `$AttrDef`  |
| 2     | 8      | `$BadClus`  |
| 3     | 6      | `$Bitmap`   |
| 4     | 7      | `$Boot`     |
| 5     | 11     | `$Extend`   |
| 6     | 2      | `$LogFile`  |
| 7     | 0      | `$MFT`      |
| 8     | 1      | `$MFTMirr`  |
| 9     | 9      | `$Quota` / `$Secure` (cluster-size-dependent, see [§2 slot 9](02-mft-records.md)) |
| 10    | 10     | `$UpCase`   |
| 11    | 3      | `$Volume`   |

`rust-fs-ntfs::mkfs` emits the same skeleton shape via
`build_skeleton_fn_stream(parent_reference, name)` for every system
entry in the root `$I30` loop except `$MFT`
[`[OBSERVED: src/mkfs.rs::build_skeleton_fn_stream]`](#references).
Without this shape, `chkdsk` Stage 1 fires
`Event 55 → file reference 0x5000000000005 → corrupted index attribute
:$I30:$INDEX_ROOT` even when every per-record `$FILE_NAME` is correct.
A missing entry or a non-skeleton FN body trips the same event.

Cross-link: [`docs/chkdsk-improvement-findings.md §2.7`](../../chkdsk-improvement-findings.md) carries the per-iteration history of how this was corroborated against `format.com`'s output.

### DIRECTORY bit on `$FILE_NAME.file_attributes` for system directories {#fn-directory-bit}

The `$FILE_NAME` attribute's `file_attributes` field at offset `0x38`
([§4 layout](#file-name-layout)) mirrors `$STANDARD_INFORMATION`'s flag
bitmask but also carries the `FILE_ATTRIBUTE_DIRECTORY` bit
(`0x10000000` in the FN-attribute encoding, not the SI `0x10`) when the
record is a directory.

The only system record that's a directory in the first 16 is the root
itself (rec 5). On the root's per-record `$FILE_NAME` the field reads:

```
file_attributes = 0x06          | 0x10000000
                  (HIDDEN+SYSTEM) (DIRECTORY for FN-mirror encoding)
                = 0x10000006
```

`format.com` ships this exact byte sequence
[`[OBSERVED: docs/mkfs-bug-catalog.md "Bug 9"]`](#references). Earlier
`rust-fs-ntfs` revisions emitted
`0x00000006` (missing the DIRECTORY bit) and Event 55 fired with the
same `corrupted index attribute :$I30:$INDEX_ROOT` message at all
cluster sizes. The fix is to OR `0x10000000` into `file_attributes`
when the record is both `is_dir` and `is_system` (or more generally:
whenever the MFT record has `$INDEX_ROOT:$I30`).

## Other named indexes {#other-indexes}

Several system files use the same `$INDEX_ROOT` / `$INDEX_ALLOCATION` /
`$BITMAP` machinery with different stream names, indexed-attribute
codes, and collation rules. Each one identifies the host record by its
fixed MFT number. Cross-references to other sections show where the
index *contents* (not just the index machinery) are described.

| Stream | Host record                     | Key                            | Value                          | Collation                    | Source                                 |
| ------ | ------------------------------- | ------------------------------ | ------------------------------ | ---------------------------- | -------------------------------------- |
| `$I30` | every directory                 | `$FILE_NAME` (var)             | (none — key is value)          | `COLLATION_FILE_NAME`        | [OBSERVED: src/record_build.rs `write_empty_index_root` line 309; src/mkfs.rs `build_populated_index_root_attr` line 2319] |
| `$SDH` | `$Secure` (rec 9)               | (security hash, security ID)   | offset into `$SDS`             | `COLLATION_NTOFS_SECURITY_HASH` (0x12) | [OBSERVED: src/mkfs.rs:1133] |
| `$SII` | `$Secure` (rec 9)               | security ID (`u32`)            | offset into `$SDS`             | `COLLATION_NTOFS_ULONG` (0x10) | [OBSERVED: src/mkfs.rs:1148]       |
| `$O`   | `$Extend\$ObjId`                | object GUID (16 B)             | MFT ref + 3×birth GUID         | `COLLATION_NTOFS_ULONGS` (0x13) | [OBSERVED: src/mkfs.rs:1306]        |
| `$O`   | `$Extend\$Quota`                | user SID (var)                 | owner ID (`u32`)               | `COLLATION_NTOFS_SID`        | [OBSERVED: src/mkfs.rs line 1304 `COLLATION_NTOFS_SID` emitted for `$Quota:$O`] |
| `$Q`   | `$Extend\$Quota`                | owner ID (`u32`)               | quota record (40 B + SID)      | `COLLATION_NTOFS_ULONG` (0x10) | [OBSERVED: src/mkfs.rs line 1294 `COLLATION_NTOFS_ULONG` emitted for `$Quota:$Q`] |
| `$R`   | `$Extend\$Reparse`              | (reparse tag, MFT ref)         | (none)                         | `COLLATION_NTOFS_ULONGS` (0x13) | [OBSERVED: src/mkfs.rs:1329]        |
| `$J`   | `$Extend\$UsnJrnl` (`$DATA`)    | — (sparse stream, not a B+ tree) | —                            | —                            | [UNVERIFIED] — covered in [§5](05-logfile-journal.md) |
| `$Max` | `$Extend\$UsnJrnl` (`$DATA`)    | — (16-byte struct, not indexed) | —                            | —                            | [UNVERIFIED] — covered in [§5](05-logfile-journal.md) |

Notes:

- **Two `$O` indexes exist**, one in `$ObjId` and one in `$Quota`,
  using different collation rules. They are not interchangeable.
- **`$J` and `$Max` are not B+ trees.** They appear in the same MFT
  file (`$UsnJrnl`) but are conventional `$DATA` streams; the names look
  index-shaped purely as a `$Extend` naming convention. Detailed layout
  is in [§5 $LogFile & journal](05-logfile-journal.md).
- **`$Reparse\$R` value layout.** The `$R` index is a reparse-tag →
  MFT-ref map; the exact value layout (MFT ref only vs. MFT ref + tag
  mirror) is not pinned down in permitted sources. [UNVERIFIED]
- **MFT_RECORD_IS_VIEW_INDEX (`0x0008`)** must be set on the host record
  for every view index in this table (i.e. all rows except `$I30`). The
  flag indicates the record exists primarily as an index host, even if
  the indexed-attribute code is `0x00` (synthetic).
  [OBSERVED: docs/chkdsk-improvement-findings.md §2.2.3, §2.4]

For the *contents* of these indexes (security descriptors, quota
records, reparse data, etc.) see [§6 Special streams](06-special-streams.md).

## Orphan recovery (read-side awareness) {#orphan-recovery}

`rust-fs-ntfs` does not implement repair, but the read path must be
aware of the *kinds* of inconsistency that produce orphaned content,
because a partially-modified volume on the host that we mount may exhibit
exactly these states.

### Two complementary orphan classes {#orphan-classes}

A file can become "orphaned" in two distinct senses:

1. **MFT-orphan** — the MFT record is `IN_USE`, has valid attributes,
   and has a `$FILE_NAME` whose `parent_directory_reference` points at
   some directory `D`, but `D`'s `$I30` does *not* contain a matching
   entry. The file is reachable from MFT enumeration but invisible to
   any `readdir` on `D`.
2. **INDX-orphan** — an INDX block exists in `$INDEX_ALLOCATION`, the
   block validates (USA passes, `INDX` magic, sane `INDEX_HEADER`), but
   the block is not reachable from the rebuilt B+ tree (the bit in
   `$BITMAP:$I30` is set yet no internal entry's `subnode_vcn` points
   at it). Entries inside such a block are invisible to a normal
   tree-walk reader.

[UNVERIFIED]

### MFT-first vs. orphan-INDX sweep {#orphan-mft-first}

For this spec the takeaway is purely descriptive — readers SHOULD treat
an `$I30` walk as authoritative for `readdir` semantics, but SHOULD NOT
assume that "every file whose `$FILE_NAME.parent_ref = D`" appears in
`D.$I30`. The two views can disagree, and Windows itself sometimes
reports the MFT-first view (`chkdsk`) and sometimes the index-first view
(Explorer) of the same volume.

### $I30 reconstruction (conceptual) {#i30-reconstruction}

The conceptual rebuild — implemented by repair tools, not by
`rust-fs-ntfs` — proceeds in four levels.

| Level | Action                                                                                              | Source                |
| ----- | --------------------------------------------------------------------------------------------------- | --------------------- |
| L1    | Linear scan of every existing entry; drop ghosts (freed MFT, out-of-range, parent mismatch)         | [UNVERIFIED]          |
| L2    | Resort the surviving entries under `COLLATION_FILE_NAME` using the volume's own `$UpCase`           | [UNVERIFIED]          |
| L3    | If node hierarchy is damaged, throw it away and rebuild the B+ tree from scratch with node splits   | [UNVERIFIED]          |
| L4    | Sweep `$INDEX_ALLOCATION` sequentially for orphan INDX pages; harvest filenames from damaged MFTs   | [UNVERIFIED]          |

The Level 1 entry-validation criteria, restated:

1. Entry size is sane (header + key + optional value + optional VCN tail
   ≤ block size).
2. `parent_directory_reference` resolves to *this* directory's MFT
   record number with a matching sequence.
3. The pointed-to MFT record exists, is `IN_USE`, is not a child
   `$ATTRIBUTE_LIST` extension record.
4. The pointed-to record contains a `$FILE_NAME` whose name matches the
   key (case-insensitive UTF-16 compare via `$UpCase`).

[UNVERIFIED]

### Hardlink count cross-check {#hardlink-crosscheck}

After any reconstruction, the MFT record's `LinkCount` must equal the
total number of `$I30` entries across the volume that point at this
record. Mismatches are repaired by *rewriting the MFT `LinkCount` to
match the on-disk index reality*, not by inserting synthetic index
entries. [UNVERIFIED]

For read-only consumers of the volume, a `LinkCount` greater than the
observed entry count usually indicates an in-progress or aborted unlink;
`LinkCount` smaller than the observed count usually indicates an
abandoned hardlink or rename. Either way, treating `LinkCount` as
strictly authoritative will cause inconsistencies under host activity.
[OBSERVED: STATUS.md "duplicate `$FILE_NAME` times … left stale"]

## References

- `rust-fs-ntfs` source: [`src/idx_block.rs`](../../../src/idx_block.rs),
  [`src/index_io.rs`](../../../src/index_io.rs),
  [`src/record_build.rs`](../../../src/record_build.rs),
  [`src/attr_io.rs`](../../../src/attr_io.rs).
- `rust-fs-ntfs` docs: [`docs/STATUS.md`](../../STATUS.md),
  [`docs/chkdsk-improvement-findings.md`](../../chkdsk-improvement-findings.md),
  [`docs/mkfs-bug-catalog.md`](../../mkfs-bug-catalog.md).
- Cross-section dependencies:
  [§2 USA fixup](02-mft-records.md#usa-fixup),
  [§3 cluster bitmap](03-data-runs-bitmap.md),
  [§5 $LogFile sequence numbers](05-logfile-journal.md),
  [§6 $UpCase contents and $Secure streams](06-special-streams.md).

## Open questions

Items in this list should also appear in
[`notes/open-questions.md`](../notes/open-questions.md) until resolved.

- [ ] (2026-05-03) `$FILE_NAME` timestamps in `$I30` entries are
      written-once at link/rename time and not synced to subsequent
      `$STANDARD_INFORMATION` updates — needs a black-box test that
      mutates SI and re-reads the parent index.
- [ ] (2026-05-03) `$FILE_NAME.allocated_size` / `real_size` snapshot
      semantics — confirm chkdsk does not flag stale values.
- [x] (2026-05-03 → 2026-05-27) Namespace value table {0,1,2,3} and the
      `Win32&DOS=3` collapse heuristic — confirmed from code: `NAMESPACE_POSIX=0`,
      `NAMESPACE_WIN32_DOS=3` (src/mkfs.rs lines 137–138); `fn_namespace_for`
      implements the 8.3 test (src/record_build.rs lines 133–155).
- [ ] (2026-05-03) `Win32 + DOS` paired entries pointing at the same
      MFT record — confirm with a formatter that emits a name violating
      8.3.
- [ ] (2026-05-03) `chkdsk` namespace-cross-validation behaviour for
      lone DOS entries — produce a synthesised volume missing the Win32
      partner and observe chkdsk's report.
- [ ] (2026-05-03) `NtfsDisable8dot3NameCreation` detection from disk
      state alone — observe a volume formatted with the flag set.
- [ ] (2026-05-03) `clusters_per_index_block` negative encoding when
      `index_block_size < cluster_size` — synthesise such a volume and
      verify chkdsk acceptance.
- [ ] (2026-05-03) `$INDEX_ALLOCATION` block-size 4096 byte default —
      confirm formatter behaviour across 512 / 1024 / 2048 / 4096
      cluster sizes.
- [ ] (2026-05-03) `$BITMAP:$I30` trailing-bit padding to byte boundary
      — confirm by inspecting a directory whose entry count is not a
      multiple of 8.
- [x] (2026-05-03 → 2026-05-27) Index-entry 8-byte alignment of body —
      confirmed from code: `build_index_entry` (src/mkfs.rs line 2260) uses
      `align8(header + stream.len())` as `entry_length`.
- [ ] (2026-05-03) INDX free-space pointer semantics — does Windows
      emit any explicit free-list within an INDX block?
- [x] (2026-05-03 → 2026-05-27) `COLLATION_NTOFS_SECURITY_HASH` (0x12) and
      `COLLATION_NTOFS_ULONGS` (0x13) numeric values — confirmed from
      src/mkfs.rs constants (lines 55, 62) with MS-FSCC §2.4 citations in
      comments and emission verified at the call sites.
- [ ] (2026-05-03) `COLLATION_UNICODE_STRING` (0x02) numeric value —
      constant not present in codebase; no emission site found; still needs
      MS-FSCC corroboration.
- [x] (2026-05-03 → 2026-05-27) `COLLATION_NTOFS_GUID` / `COLLATION_BINARY`
      for `$ObjId\$O` — confirmed from code: `$ObjId:$O` uses
      `COLLATION_NTOFS_ULONGS` (0x13) per src/mkfs.rs line 1219.
      `COLLATION_BINARY` (0x00) is not emitted by any path in this codebase.
- [ ] (2026-05-03) `COLLATION_FILE_NAME` does not perform Unicode
      normalisation — confirm with NFC vs NFD identical text under
      `$UpCase`.
- [ ] (2026-05-03) Surrogate-pair ordering under `COLLATION_FILE_NAME`
      — code-unit order vs code-point order test case.
- [x] (2026-05-03 → 2026-05-27) Absence of `.`/`..` in `$I30` — confirmed
      from code: the root `$I30` builder loop (src/mkfs.rs lines 1346–1374)
      emits no `.` or `..` key entries; parent reference is encoded only in
      `$FILE_NAME.parent_directory_reference`.
- [ ] (2026-05-03) `$Reparse\$R` value layout — observe in a volume
      that has live reparse points.
- [ ] (2026-05-03) Level-1 "7 strict validation criteria" for index
      entries — only four of the seven (size / parent-ref / MFT-status /
      `$FILE_NAME`-match) are pinned in our notes; the remaining three
      need a permitted-source citation.

[← Prev: Data runs & cluster allocation](03-data-runs-bitmap.md) | [TOC](../ntfs-specification.md) | [Next: $LogFile & journal →](05-logfile-journal.md)
