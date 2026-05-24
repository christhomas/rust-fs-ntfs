[← Prev: Volume geometry & boot sector](01-geometry-boot.md) | [TOC](../ntfs-specification.md) | [Next: Data runs & cluster allocation →](03-data-runs-bitmap.md)

# 2. MFT & records

The Master File Table (MFT) is the on-disk inventory of every object on an NTFS
volume. Every file, directory, system metadata stream, and free slot is one or
more MFT records. This section covers record sizing, the FILE record header,
the multi-sector Update Sequence Array (USA) "fixup" mechanism, the redundant
mirror in `$MFTMirr`, the first 16 system files, the resident vs non-resident
attribute split, and the rules around sequence numbers, attribute IDs, and
extension records via `$ATTRIBUTE_LIST`.

Cross-section concerns are handled in their own pages:

- Data run encoding (mapping pairs) — [§3 Data runs & cluster allocation](03-data-runs-bitmap.md)
- `$INDEX_ROOT`, `$INDEX_ALLOCATION`, `$BITMAP` for directories — [§4 Indexes & directories](04-indexes-directories.md)
- `$LogFile` internals — [§5 $LogFile & journal](05-logfile-journal.md)
- Compression, alternate data streams, EAs, reparse points — [§6 Special streams](06-special-streams.md)

## Overview {#overview}

The MFT itself is a file: MFT record 0 (`$MFT`) describes its own `$DATA`
attribute, whose data runs locate every other MFT record on disk. The first
16 records are reserved for filesystem-internal "system files"; user files
start at record 16. The volume's [boot sector](01-geometry-boot.md) carries
an LCN pointer (`mft_lcn`, offset `0x30`) to the first cluster of `$MFT`, plus
the per-record size encoded at offset `0x40`. [OBSERVED: src/mft_io.rs:91-100]

**Record size encoding.** The byte at boot-sector offset `0x40`
(`clusters_per_mft_record`) is signed. Positive values mean "this many
clusters per record"; negative values mean `2^|value|` bytes per record. So
`0xF6 = -10 → 1024-byte records` and `0xF4 = -12 → 4096-byte records`. The
same encoding rule applies to offset `0x44` for `$INDEX_ALLOCATION` block
size. [OBSERVED: src/mft_io.rs:95-100]

A common pitfall is hardcoding `1024` as the record size: real volumes
routinely format with 4096-byte records, and a 4 KiB record exposes a larger
USA than a 1 KiB record (the USA grows linearly with `record_size /
bytes_per_sector`). Hardcoding 1024 misplaces the first attribute and silently
clobbers user data. [OBSERVED: docs/mkfs-bug-catalog.md Bug 8 — `ATTRS_OFFSET` hardcoded to 0x38]

**MFT-record offset on disk.** For record `n`:

```
record_byte_offset = mft_lcn * cluster_size + n * file_record_size
```

[OBSERVED: src/mft_io.rs:117-119]

`rust-fs-ntfs` validates the parsed `file_record_size` is in `[512, 16384]`
before any addressing arithmetic; out-of-range values are rejected at boot
parse. [OBSERVED: src/mft_io.rs:101-105]

**Allocation tracking.** Whether record `n` is currently in use is tracked by
a `$BITMAP` attribute on `$MFT` itself (one bit per record, 1 = in use).
On small volumes this bitmap is resident inside record 0; on larger volumes
it spills into a non-resident attribute with its own data runs.
[OBSERVED: src/mft_bitmap.rs]

The same `IN_USE` state is also encoded in each record's own header `flags`
field at offset `0x16` (bit `0x0001`). The bitmap and the per-record flag are
expected to agree. [UNVERIFIED]

## MFT record header {#record-header}

Every MFT record opens with a fixed-layout header followed by the Update
Sequence Array, then 8-byte-aligned attribute records, then an 8-byte
end-of-attributes sentinel, then unused space up to `bytes_allocated`.
[OBSERVED: src/record_build.rs:269-321]

The header layout, with byte offsets relative to record start:

| Offset | Size | Field                          | Meaning                                                                                |
| ------:| ----:| ------------------------------ | -------------------------------------------------------------------------------------- |
| `0x00` | 4    | Magic                          | ASCII `FILE` (`46 49 4C 45`); INDX blocks use `INDX`, log blocks `RCRD` / `RSTR`        |
| `0x04` | 2    | USA offset                     | Byte offset from record start to the first byte of the Update Sequence Array            |
| `0x06` | 2    | USA count                      | `(record_size / bytes_per_sector) + 1` — one USN word plus one save-word per sector     |
| `0x08` | 8    | `$LogFile` Sequence Number     | LSN of the most recent journal entry that mutated this record                          |
| `0x10` | 2    | Sequence number                | Bumped each time the record is reallocated; combined with record number → file ref     |
| `0x12` | 2    | Hard link count                | Number of `$FILE_NAME` attributes pointing at this record                              |
| `0x14` | 2    | First attribute offset         | Byte offset from record start to the first attribute record                            |
| `0x16` | 2    | Flags                          | `0x0001 IN_USE`, `0x0002 IS_DIRECTORY`, `0x0008 IS_VIEW_INDEX`                         |
| `0x18` | 4    | Used size (`bytes_used`)       | Offset of the first free byte after the end-of-attributes marker                       |
| `0x1C` | 4    | Allocated size                 | Total record size in bytes (matches `file_record_size` from boot)                      |
| `0x20` | 8    | Base file reference            | `0` for a base record; otherwise the file reference of the record that owns this one   |
| `0x28` | 2    | Next attribute ID              | First unused attribute instance ID for this base record                                |
| `0x2A` | 2    | (alignment / pad — varies by version) |                                                                                 |
| `0x2C` | 4    | MFT record number              | Self-reference; present in NTFS 3.1 records (see version note below)                   |

**FILE magic.** `rust-fs-ntfs` rejects any record whose first four bytes are
not `FILE`. This catches both wholly-uninitialised slots (zeros) and slots
overwritten by foreign data. [OBSERVED: src/mft_io.rs:153-159]

**USA offset / count.** These two fields drive the per-sector "fixup"
described in [§2.3 USA fixup](#usa-fixup). The USA must fit in the record
between the header and the first attribute, and the array length must equal
exactly `(record_size / bytes_per_sector) + 1` (one update-sequence-number
word plus one save-word per protected sector). `rust-fs-ntfs` validates this
geometry on every read. [OBSERVED: src/mft_io.rs:240-266]

**LSN at `0x08`.** The `$LogFile` Sequence Number is the on-disk pointer
linking this record's most recent mutation to a log record in `$LogFile`.
A record that has never been logged carries LSN = 0; freshly built records
emitted by `rust-fs-ntfs` initialise it to 0. [OBSERVED: src/record_build.rs:117]
[UNVERIFIED] — see [§5 $LogFile & journal](05-logfile-journal.md) for
replay-time meaning.

**Sequence number at `0x10`.** A 16-bit counter, incremented every time the
slot is reallocated to a new logical file. Combined with the 48-bit record
number, it forms the 64-bit "file reference" used everywhere a child record
points at its parent or an `$ATTRIBUTE_LIST` entry points at an extension
record. See [§2.11 Sequence numbers and reuse](#sequence-numbers).
[OBSERVED: src/record_build.rs:17-19]

**Hard link count at `0x12`.** Equal to the number of `$FILE_NAME` attributes
in the record (one per directory entry that points at this file). A regular
file with one parent has hard link count 1; a hard-linked file has 2 or
more. A record with **no** `$FILE_NAME` at all (e.g. a placeholder MFT
slot reserved by the formatter but never wired into the directory tree —
see §2 records 11..15) carries `link_count = 0`; a non-zero value here on
a record with no FN attribute would be flagged by `chkdsk /F`, which
post-process clears it. [OBSERVED: src/record_build.rs:119]
[OBSERVED: docs/mkfs-bug-catalog.md placeholder records (slots 11..15)]

**First attribute offset at `0x14`.** The first attribute record begins at
`align8(USA_offset + 2 + sectors * 2)`. `rust-fs-ntfs` computes this
per-record at build time:

```rust
let attrs_offset = align8(USA_OFFSET + 2 + sectors * 2);
```

[OBSERVED: src/record_build.rs:281, src/record_build.rs:122]

A historical bug in `rust-fs-ntfs` hardcoded this to `0x38`, which is correct
only for 1024-byte records (sectors = 2 → USA spans 6 bytes → `align8(0x36) =
0x38`). For 4096-byte records at 512-byte sectors the USA spans
`0x30..0x42`, so attributes must start at `align8(0x42) = 0x48`; the
hardcoded `0x38` collided with USA save-word slots and `apply_fixup_on_write`
silently clobbered the freshly-written `$STANDARD_INFORMATION`.
[OBSERVED: docs/mkfs-bug-catalog.md Bug 8]

**Flags at `0x16`.** A 16-bit bitfield:

| Bit      | Name                          | Meaning                                                                                |
| -------- | ----------------------------- | -------------------------------------------------------------------------------------- |
| `0x0001` | `MFT_RECORD_IN_USE`           | Record is currently allocated; clear ⇒ slot is free                                    |
| `0x0002` | `MFT_RECORD_IS_DIRECTORY`     | Record hosts a `$FILE_NAME`-keyed `$I30` index — i.e. an ordinary directory             |
| `0x0004` | (reserved)                    | Not used                                                                               |
| `0x0008` | `MFT_RECORD_IS_VIEW_INDEX`    | Record hosts a *named view index* indexing something other than `$FILE_NAME`           |

The view-index bit is required on `$Secure` (which carries `$SDH` / `$SII`),
on `$Quota` (`$O` / `$Q`), on `$ObjId` (`$O`), and on `$Reparse` (`$R`).
chkdsk fires `Flags for file record segment N are incorrect` when the bit is
absent on a known view-index host, even if the on-disk view-index attributes
themselves are present and structurally valid.
[OBSERVED: docs/mkfs-bug-catalog.md Bug 4]
[OBSERVED: docs/chkdsk-improvement-findings.md §2.2.3]

`rust-fs-ntfs` refuses to issue a write to any record whose `IN_USE` bit is
clear at read-time; the read-modify-write primitive returns an error rather
than silently re-using a free slot. [OBSERVED: src/mft_io.rs:315-319]

**Used / allocated sizes at `0x18` / `0x1C`.** `bytes_used` is the byte offset
of the first free byte past the end-of-attributes sentinel; `bytes_allocated`
matches `file_record_size` from the boot sector. The end-of-attributes
sentinel is itself an 8-byte attribute-record header (4-byte type
`0xFFFFFFFF` + 4-byte length `0`), so `bytes_used = end_marker_offset + 8`.
A common bug is to advance the cursor by 4 after writing only the type-code
half, leaving `bytes_used = end_marker_offset + 4`; chkdsk reports `First
free byte offset corrected in file record segment N`. [OBSERVED: docs/mkfs-bug-catalog.md Bug 2]
[OBSERVED: docs/chkdsk-improvement-findings.md §2.2.1]

**Base file reference at `0x20`.** A 64-bit reference combining a 48-bit MFT
record number (low) and a 16-bit sequence number (high):

```rust
pub fn encode_file_reference(record_number: u64, sequence: u16) -> u64 {
    (record_number & 0x0000_FFFF_FFFF_FFFF) | ((sequence as u64) << 48)
}
```

[OBSERVED: src/record_build.rs:17-19]

A base record stores `0` here (it is its own base). An extension record
stores a non-zero file reference pointing at the base record that owns it,
i.e. the record whose `$ATTRIBUTE_LIST` enumerates this extension.
[UNVERIFIED]

**Next attribute ID at `0x28`.** A 16-bit counter giving the smallest unused
attribute instance ID for new attributes added to this record. Each
attribute's own header at `+0x0E` carries its assigned ID (see
[§2.7 Common attribute header](#attr-header)); `next_attr_id` is bumped each
time a new attribute is appended. `rust-fs-ntfs`'s freshly built records use
IDs `0`, `1`, `2` for `$STANDARD_INFORMATION`, `$FILE_NAME`, `$DATA` and
initialise `next_attr_id = 3`. [OBSERVED: src/record_build.rs:130-131, 290-291]

**MFT record number at `0x2C`.** A 32-bit self-reference. Present in NTFS 3.1
records; on older versions the bytes at `+0x2C..+0x30` are reserved /
zero-filled. `rust-fs-ntfs` always writes the record number on creation.
[OBSERVED: src/record_build.rs:131, 292]
[UNVERIFIED] — the format documentation we have stops at offset `0x32` and
does not explicitly mark `0x2C` as version-gated; we treat the field as
3.1-only by convention pending corroboration from `[MS-FSCC]`.

### A worked example header

Canonical 1024-byte example: [UNVERIFIED]

```
Offset  Hex                                              Field
------  -----------------------------------------------  -------------------------
0x000   46 49 4C 45                                      Magic: "FILE"
0x004   30 00                                            USA offset: 0x0030
0x006   03 00                                            USA count: 3 (1 USN + 2 save-words)
0x008   XX XX XX XX XX XX XX XX                          $LogFile Sequence Number (LSN)
0x010   01 00                                            Sequence number: 1
0x012   01 00                                            Link count: 1
0x014   38 00                                            First attribute offset: 0x0038
0x016   01 00                                            Flags: 0x0001 = IN_USE
0x018   A8 01 00 00                                      Used size: 0x01A8 (424 bytes)
0x01C   00 04 00 00                                      Allocated size: 0x0400 (1024 bytes)
0x020   00 00 00 00 00 00 00 00                          Base record reference: 0 (this IS the base)
0x028   XX XX                                            Next attribute ID
0x030   XX XX                                            USA sequence value
0x032   XX XX XX XX                                      USA fixup array (2 entries)
```

[UNVERIFIED]

For a 4096-byte record the USA stretches to 9 words (1 USN + 8 save-words),
the first attribute lives at `0x48`, and `bytes_allocated` is `0x1000`.
[OBSERVED: src/record_build.rs:281] [OBSERVED: docs/mkfs-bug-catalog.md Bug 8]

## Update Sequence Array (USA) {#usa-fixup}

NTFS multi-sector records — every MFT record, every `INDEX_ALLOCATION` (INDX)
block, and every `$LogFile` `RSTR` / `RCRD` block — carry an in-band
torn-write detector called the Update Sequence Array. The mechanism replaces
the last two bytes of every 512-byte sector inside the record with a single
common Update Sequence Number (USN), saving the original last-bytes in a
small array near the record header. On read-back the array is reversed; on
write the USN is bumped first. A torn write (some sectors landed, some did
not) shows up as a sector whose tail bytes do not match the others, and the
fixup raises an error. [OBSERVED: src/mft_io.rs:30-36]

This same scheme is reused outside the MFT — see [§4 Indexes & directories](04-indexes-directories.md#indx-fixup)
for INDX blocks and [§5 $LogFile & journal](05-logfile-journal.md#log-block-fixup)
for RSTR / RCRD blocks. The byte mechanics are identical; only the magic
signature and the protected-block size differ.

### Geometry

Given a record of size `record_size` and a sector size `bytes_per_sector`:

```
sectors      = record_size / bytes_per_sector
usa_count    = sectors + 1                 // 1 USN word + 1 save-word per sector
usa_size     = usa_count * 2               // bytes
```

The USA lives at `usa_offset` (header offset `0x04` of the record) and spans
`[usa_offset, usa_offset + usa_size)`. Attributes start at the first 8-byte
boundary after that. [OBSERVED: src/mft_io.rs:240-266]

For the two common record sizes:

| `record_size` | `bytes_per_sector` | sectors | `usa_count` | USA span        | `attrs_offset` |
| -------------:| ------------------:| -------:| -----------:| ---------------:| --------------:|
| 1024          | 512                | 2       | 3           | `0x30..0x36`    | `0x38`         |
| 4096          | 512                | 8       | 9           | `0x30..0x42`    | `0x48`         |

[OBSERVED: src/record_build.rs:122, 281]
[OBSERVED: docs/mkfs-bug-catalog.md Bug 8 / docs/chkdsk-improvement-findings.md §2.2.4]

### On-disk layout

```
record start
   ├── header [0x00..usa_offset)
   ├── USA
   │     ├── word 0:    USN value (the "sentinel")
   │     ├── word 1:    saved last-2-bytes of sector 0
   │     ├── word 2:    saved last-2-bytes of sector 1
   │     └── …          (one save-word per sector)
   ├── padding to 8-byte boundary
   ├── attribute records
   ├── end-of-attributes sentinel (8 bytes)
   └── unused space up to `bytes_allocated`
```

The last two bytes of every protected sector — i.e. `record[(i+1) *
bytes_per_sector - 2 .. (i+1) * bytes_per_sector]` for `i in 0..sectors` —
contain the USN value when the record is at rest on disk. The original
bytes that *should* live there are saved in `USA[1 + i]`.
[OBSERVED: src/mft_io.rs:163-179]

### Read path: `apply_fixup_on_read`

1. Read `usa_offset` and `usa_count` from header offsets `0x04` and `0x06`.
2. Validate `usa_count == sectors + 1` and that the USA fits within the
   record. [OBSERVED: src/mft_io.rs:240-266]
3. Read the USN sentinel from `USA[0]` (= `record[usa_offset .. usa_offset + 2]`).
4. For each protected sector `i in 0..sectors`:
   - Compute `sector_end = (i + 1) * bytes_per_sector`.
   - Compare `record[sector_end - 2 .. sector_end]` against the USN sentinel.
   - If they differ → **fail**: torn write or corrupt record.
     `rust-fs-ntfs` returns a `String` describing the offending sector.
     [OBSERVED: src/mft_io.rs:165-174]
   - If they match, replace `record[sector_end - 2 .. sector_end]` with the
     saved bytes from `USA[1 + i]`.
5. After all sectors are reverted, the record is ready for attribute parsing.

[OBSERVED: src/mft_io.rs:148-180]

### Write path: `apply_fixup_on_write`

Inverse of the read path; called immediately before writing the record back
to disk:

1. Read the current USN from `USA[0]`.
2. Bump it by 1 with wrap-around, **skipping zero**. Some NTFS drivers
   treat USN = 0 as "uninitialised", so `rust-fs-ntfs` skips that value:

   ```rust
   let new_usn = match old_usn.wrapping_add(1) {
       0 => 1,
       n => n,
   };
   ```

   [OBSERVED: src/mft_io.rs:208-213]
3. Write `new_usn` back to `USA[0]`.
4. For each protected sector `i in 0..sectors`:
   - Save the current `record[sector_end - 2 .. sector_end]` bytes into
     `USA[1 + i]`.
   - Overwrite `record[sector_end - 2 .. sector_end]` with `new_usn`.

[OBSERVED: src/mft_io.rs:216-224]

### Strict validation policy

`rust-fs-ntfs` does not attempt heuristic recovery of a USA mismatch: a USA
mismatch is reported as an error and the caller is expected to either fail
the operation or, on a `$MFTMirr`-eligible record (numbers 0..N where N is
bounded by the mirror's data run), fall back to the mirror copy. See
[§2.4 $MFT and $MFTMirr](#mft-mirror). [OBSERVED: src/mft_io.rs:165-174]

### Why fixup ordering matters for CRC

NTFS 3.1+ MFT records carry a CRC32 checksum in their last 4 bytes. The CRC
is computed *after* the USA fixup is applied to the in-memory buffer (i.e.
on the post-revert bytes), not on the raw on-disk bytes. Validating CRC
before reverting the USA produces guaranteed false positives, because the
sector-tail bytes will all carry the USN sentinel rather than their original
contents.
[UNVERIFIED] — `rust-fs-ntfs` does not currently validate or emit this CRC32;
the read path stops at USA validation. [OBSERVED: src/mft_io.rs:148-180]

### Concurrency contract

The `update_mft_record` primitive is read-modify-write: it reads the record,
applies fixup, calls a mutator on the clean bytes, re-applies fixup, and
writes the record back. **It is not safe under concurrent writers**: any
external write that lands between the read and the write tears the update.
Callers must arrange that no other process mutates the image during the
call. Within a single fs-ntfs process this is upheld by the absence of
internal threads; across processes (the volume mounted on Windows or by a
second fs-ntfs caller) it is the operator's responsibility to quiesce.
[OBSERVED: src/mft_io.rs:5-23]

## $MFT (#0) and $MFTMirr (#1) {#mft-mirror}

`$MFT` (record 0) is the master file table itself. Its `$DATA` attribute is
non-resident, with data runs spanning the clusters that physically hold every
MFT record (including record 0's own first cluster).
[OBSERVED: src/mft_bitmap.rs:1-13]

`$MFTMirr` (record 1) is a partial mirror. It contains a `$DATA` attribute
whose data runs cover a small contiguous region near the volume midpoint;
that region holds byte-identical copies of the **first N MFT records**, where
N is the count of records that fits in the mirror's allocated size, **not**
a hardcoded 4. [UNVERIFIED]

> **Range Limitation Rule (CRITICAL):** The `$MFTMirr` is NOT a complete mirror
> of the `$MFT`. The utility MUST NOT hardcode a limit of 0-3. Instead, it
> MUST calculate the mirrored range dynamically from the `$MFTMirr` Data Run
> size (e.g., if the run is 32KB and records are 4KB, it mirrors records 0
> to 7).

[UNVERIFIED]

So a 32 KiB mirror at 4 KiB records carries records 0..7; at 1 KiB records
the same 32 KiB carries records 0..31. The on-disk layout is otherwise
identical to `$MFT`'s — same record size, same USA fixup, same headers.

### Repair / divergence semantics

**Decision matrix** for system records 0..3 where `$MFTMirr` is available:

| Primary state                 | Mirror state                  | Action                             |
| ----------------------------- | ----------------------------- | ---------------------------------- |
| Struct fail (USA / CRC)       | Valid                         | Copy mirror → primary              |
| Struct fail                   | Struct fail                   | FATAL (record 0) or mark FREE      |
| Valid / semantic fail         | Valid / valid                 | Copy mirror → primary              |
| Valid / semantic fail         | Valid / semantic fail         | Mark FREE → orphan recovery        |
| Valid / valid                 | Struct fail                   | Copy primary → mirror              |
| Valid / valid                 | Valid / semantic fail         | Copy primary → mirror              |

[UNVERIFIED]

`rust-fs-ntfs` does not yet implement repair; the read path simply surfaces
USA failures as errors and the writer never touches `$MFTMirr` itself.
[OBSERVED: docs/STATUS.md] [UNVERIFIED] — the repair semantics above are
not exercised by our test suite.

### What the mirror does NOT cover

A common misconception worth flagging: `$MFTMirr` only mirrors a small prefix
of `$MFT`. For records beyond the mirrored range, there is no redundant copy,
and a primary failure must fall back to orphan recovery (rebuilding the
file's identity from `$ATTRIBUTE_LIST` entries, parent index entries, and
hard-link counts in surviving records). [UNVERIFIED]

## The 16 system files (#0–#15) {#system-files}

NTFS reserves the first 16 MFT records for filesystem-internal metadata
files. Each is a real file with a `$FILE_NAME` linking it from the root
directory; tools that walk the MFT see them as ordinary files (with the
`HIDDEN | SYSTEM` attribute bits) sitting in the volume root.

| #     | Name          | Purpose                                                                             | Mandatory in |
| -----:| ------------- | ----------------------------------------------------------------------------------- | ------------ |
| 0     | `$MFT`        | The MFT itself; data runs in `$DATA` locate every other record                      | 1.2 / 3.0 / 3.1 |
| 1     | `$MFTMirr`    | Partial mirror of the first N records of `$MFT`                                     | 1.2 / 3.0 / 3.1 |
| 2     | `$LogFile`    | Transactional log; see [§5](05-logfile-journal.md)                                  | 1.2 / 3.0 / 3.1 |
| 3     | `$Volume`     | Volume label, version, dirty flag (`$VOLUME_NAME` 0x60, `$VOLUME_INFORMATION` 0x70) | 1.2 / 3.0 / 3.1 |
| 4     | `$AttrDef`    | Attribute-type-code definitions table                                               | 1.2 / 3.0 / 3.1 |
| 5     | `.` (root)    | Root directory; carries the populated `$INDEX_ROOT:$I30`                            | 1.2 / 3.0 / 3.1 |
| 6     | `$Bitmap`     | Cluster allocation bitmap for the whole volume                                      | 1.2 / 3.0 / 3.1 |
| 7     | `$Boot`       | Backed by sector 0 (and the backup boot sector) of the volume                       | 1.2 / 3.0 / 3.1 |
| 8     | `$BadClus`    | Sparse `$DATA:$Bad` whose runs cover known bad clusters                             | 1.2 / 3.0 / 3.1 |
| 9     | `$Secure` *or* `$Quota` | Security descriptor store (3.0+) **or** legacy `$Quota` slot                | 3.0 / 3.1 (slot reused; see note) |
| 10    | `$UpCase`     | Unicode uppercase-mapping table used for case-insensitive collation                 | 1.2 / 3.0 / 3.1 |
| 11    | `$Extend`     | Directory containing 3.0+ extension files (`$ObjId`, `$Quota`, `$Reparse`, `$UsnJrnl`) | 3.0 / 3.1 |
| 12–15 | (reserved)    | Reserved; implementations may leave these unused or use them as a small pool        | —            |

[OBSERVED: docs/mkfs-bug-catalog.md Bug 5 (root `$I30` enumerates 11 system records + `.`)]
[OBSERVED: docs/chkdsk-improvement-findings.md §2.5.1 (per-record attribute set table for records 0..10)]

**Slot 9 — `$Secure` vs `$Quota`** (corroborated 2026-05-23). The
`$FILE_NAME` `chkdsk` expects at slot 9 is **cluster-size-dependent**:

| `cluster_size` | Slot 9 name | Notes                                                                                          |
| -------------- | ----------- | ---------------------------------------------------------------------------------------------- |
| `< 4096`       | `$Quota`    | Modern NTFS-3.x convention. `chkdsk`'s non-4K cluster path runs a slot-9-name check.           |
| `≥ 4096`       | `$Secure`   | `chkdsk`'s 4K-cluster path does **not** run the slot-9-name check; either spelling is accepted. |

At cluster sizes 512 and 1024, shipping `$Secure` at slot 9 produces
the diagnostic sequence:

```
The file name in system file record segment 9 contains errors.
Stage 2: Examining file name linkage ...
Deleting invalid system file name $Secure (9) in directory 5.
Repairing invalid system file name $Quota (9) in directory 5.
Correcting system file name errors in file 9.
Error detected in index $I30 for file 5.
Index entry $Secure in index $I30 of file 5 is incorrect.
```

`rust-fs-ntfs::mkfs` switches the slot 9 file name accordingly via
`rec::name(rec::SECURE, cluster_size)`
[`[OBSERVED: src/mkfs.rs:342-348]`](#references). The internal record-
slot constant is still spelled `SECURE`; only the on-disk
`$FILE_NAME` text varies. Regardless of which spelling occupies slot
9, the `MFT_RECORD_IS_VIEW_INDEX` bit (`0x0008`) is required — both
`$Quota` and `$Secure` host view indexes
[`[OBSERVED: docs/mkfs-bug-catalog.md Bug 4]`](#references),
[`[OBSERVED: docs/chkdsk-improvement-findings.md §2.2.3]`](#references).

**Version dependencies.** NTFS 3.0 (Windows 2000) introduced `$Extend` and
its descendants. The `$Volume`'s `$VOLUME_INFORMATION` attribute carries
the `MajorVersion` / `MinorVersion` bytes; NTFS 1.2 volumes lack `$Extend`,
$Secure, the 24-byte extension on `$STANDARD_INFORMATION`, and the per-record
CRC32. [UNVERIFIED]
See [§1 Volume geometry](01-geometry-boot.md#ntfs-versions) for the
authoritative version table; per-section feature deltas are recorded
locally in each affected section.

**Per-record attribute set, post-`format.com` reference.** Empirically
observed on a Microsoft-formatted reference image:

| Rec | Name        | Attribute types present (besides 0x10 `$STD_INFO` and 0x30 `$FILE_NAME`)              |
| ---:| ----------- | ------------------------------------------------------------------------------------- |
| 0   | `$MFT`      | `0x50` SD, `0x80` `$DATA`, `0xB0` `$BITMAP`                                           |
| 1   | `$MFTMirr`  | `0x50` SD, `0x80` `$DATA`                                                             |
| 2   | `$LogFile`  | `0x50` SD, `0x80` `$DATA`                                                             |
| 3   | `$Volume`   | `0x50` SD, `0x60` `$VOLUME_NAME`, `0x70` `$VOLUME_INFORMATION`, `0x80` `$DATA`         |
| 4   | `$AttrDef`  | `0x50` SD, `0x80` `$DATA`                                                             |
| 5   | `.` (root)  | `0x50` SD (ROOT variant), `0x90:$I30` `$INDEX_ROOT`                                    |
| 6   | `$Bitmap`   | `0x50` SD, `0x80` `$DATA`                                                             |
| 7   | `$Boot`     | `0x50` SD, `0x80` `$DATA`                                                             |
| 8   | `$BadClus`  | `0x50` SD, `0x80` `$DATA`, `0x80:$Bad` named `$DATA`                                   |
| 9   | `$Secure`   | `0x50` SD, `0x80` `$DATA` (plus view indexes `$SDH` / `$SII`)                          |
| 10  | `$UpCase`   | `0x50` SD, `0x80` `$DATA`                                                             |

[OBSERVED: docs/chkdsk-improvement-findings.md §2.5.1]

The `$SECURITY_DESCRIPTOR` (`0x50`) is present on every system record in the
reference output; absence on system records is a known mkfs divergence
tracked in [docs/mkfs-bug-catalog.md](../../mkfs-bug-catalog.md) and currently
correlates with an `frs.cxx 0x60f` chkdsk internal assertion plus Windows
`ERROR_NO_SYSTEM_RESOURCES` on writes. [OBSERVED: docs/mkfs-bug-catalog.md Outstanding]

## Resident vs non-resident attributes {#resident-nonresident}

Every attribute lives in exactly one of two forms.

A **resident attribute** stores its value bytes inside the MFT record
itself, immediately after the attribute's own header (and after its name, if
it has one). Used for small, bounded values: `$STANDARD_INFORMATION`,
`$FILE_NAME`, `$VOLUME_NAME`, `$VOLUME_INFORMATION`, `$OBJECT_ID`, small
`$INDEX_ROOT` bodies, `$EA_INFORMATION`, and small `$DATA` (typical break-even
on a 1024-byte record is around 700 bytes of data).

A **non-resident attribute** stores its value bytes in clusters out on the
volume, and the MFT record carries only a header plus an encoded list of
"data runs" (mapping pairs) that say which clusters hold which logical bytes.
Used for any value too large to fit inside a record: most `$DATA` attributes,
`$INDEX_ALLOCATION`, `$BITMAP` on large volumes, the volume `$Bitmap`'s data,
and so on. Data run encoding details belong to [§3](03-data-runs-bitmap.md).

The discriminator is byte `+0x08` of the attribute header (the
`non_resident` flag): `0` = resident, `1` = non-resident.
[OBSERVED: src/attr_io.rs:131-149]

### Migration semantics

When a resident attribute's value grows past the record's free space,
implementations migrate it to non-resident form: allocate clusters, write
the value, replace the resident header with a non-resident header carrying
data runs. The reverse migration (non-resident → resident shrink-back) is
*allowed* by the format. [UNVERIFIED]

`rust-fs-ntfs`'s normal write path does perform the resident → non-resident
migration when a `$DATA` value grows past what fits resident; the inverse
direction is not implemented. [OBSERVED: src/attr_resize.rs] [OBSERVED: docs/STATUS.md]

Existing volumes can have non-resident attributes whose value would fit
resident — there is no format invariant that forbids it. [UNVERIFIED]

## Common attribute header {#attr-header}

Every attribute record opens with a 16-byte common header, followed by either
8 more resident-fields bytes or a longer non-resident block, then optionally
the attribute's name (UTF-16 LE), then the value (resident) or mapping pairs
(non-resident), all padded to 8-byte alignment. [OBSERVED: src/attr_io.rs:131-149,
src/record_build.rs:346-367]

### Common 16-byte header

| Offset | Size | Field           | Meaning                                                                                  |
| ------:| ----:| --------------- | ---------------------------------------------------------------------------------------- |
| `0x00` | 4    | Type            | NTFS attribute type code (`0x10` = `$STANDARD_INFORMATION`, `0x30` = `$FILE_NAME`, etc.) |
| `0x04` | 4    | Length          | Total attribute size in bytes, multiple of 8                                             |
| `0x08` | 1    | Non-resident    | `0` = resident, `1` = non-resident                                                       |
| `0x09` | 1    | Name length     | UTF-16 code units (not bytes); `0` for unnamed                                           |
| `0x0A` | 2    | Name offset     | Byte offset from attribute start to the UTF-16 name; meaningful only if name length > 0  |
| `0x0C` | 2    | Flags           | `0x0001` Compressed, `0x4000` Encrypted, `0x8000` Sparse                                 |
| `0x0E` | 2    | Attribute ID    | Per-record instance ID assigned at create time                                           |

[OBSERVED: src/attr_io.rs:131-149]

The `length` field must be a multiple of 8, must be > 0, and must keep the
attribute fully inside `bytes_used`. `rust-fs-ntfs` enforces all three on
read; a violation terminates iteration silently rather than reading past
the record. [OBSERVED: src/attr_io.rs:204-213]

The flags at `0x0C` are mutually informative with the flags carried inside
non-resident headers — for example, a `Sparse` flag on a `$DATA` attribute
must agree with the presence of sparse runs in the mapping pairs.
[UNVERIFIED]

### Resident-only fields (header `0x10..0x18`)

| Offset | Size | Field          | Meaning                                                                |
| ------:| ----:| -------------- | ---------------------------------------------------------------------- |
| `0x10` | 4    | Value length   | Length of the resident value in bytes                                  |
| `0x14` | 2    | Value offset   | Byte offset from attribute start to the resident value                 |
| `0x16` | 1    | Indexed flag   | `1` if the attribute is referenced from an index                       |
| `0x17` | 1    | (reserved)     |                                                                        |

[OBSERVED: src/attr_io.rs:140-141]

The `indexed_flag` at `+0x16` is `1` for every `$FILE_NAME` (since each is
referenced from the parent directory's `$I30` index) and `0` for most other
resident attributes. Setting it to `0` on `$FILE_NAME` causes chkdsk to
report `Attribute record (30, "") from file record segment N is corrupt`.
[OBSERVED: docs/mkfs-bug-catalog.md Bug 1]
[OBSERVED: docs/chkdsk-improvement-findings.md §2.4.1]

### Non-resident-only fields (header `0x10..0x40`)

| Offset | Size | Field                  | Meaning                                                                 |
| ------:| ----:| ---------------------- | ----------------------------------------------------------------------- |
| `0x10` | 8    | First VCN              | First Virtual Cluster Number covered by this attribute                  |
| `0x18` | 8    | Last VCN               | Last VCN covered (inclusive); for empty data, set to `-1`                |
| `0x20` | 2    | Mapping pairs offset   | Byte offset from attribute start to the data-run encoding                |
| `0x22` | 2    | Compression unit       | `0` for non-compressed; otherwise compression-block exponent             |
| `0x24` | 4    | (reserved)             |                                                                         |
| `0x28` | 8    | Allocated length       | Bytes of clusters allocated to the attribute                            |
| `0x30` | 8    | Data length            | Logical length of the attribute's value in bytes                        |
| `0x38` | 8    | Initialized length     | Bytes from `0` that are initialised; the rest is implicit zero           |

[OBSERVED: src/attr_io.rs:142-148, src/record_build.rs:537-569]

When an attribute extends across multiple base + extension records, each
fragment uses its own `first_vcn` / `last_vcn` to describe the slice it
holds. The `$ATTRIBUTE_LIST` (see [§2.10](#attribute-list)) records the
mapping from VCN ranges to extension records.

### Attribute name

If `name_length > 0`, the UTF-16 LE name lives at `attr_offset + name_offset`
and is `name_length * 2` bytes. The most common named attributes are:

- `$DATA:$Bad` — bad-cluster sparse stream on `$BadClus`
- `$DATA:<stream-name>` — alternate data streams on user files (see [§6](06-special-streams.md#alternate-data-streams))
- `$INDEX_ROOT:$I30` — the directory index root on every directory
- `$INDEX_ALLOCATION:$I30` — the non-resident extension of the same index
- `$BITMAP:$I30` — index-allocation bitmap on directories that have one

[OBSERVED: src/attr_io.rs:107-127, src/record_build.rs:478-515]

`rust-fs-ntfs`'s `attr_name_equals` decodes the on-disk UTF-16 LE bytes via
Rust's `String::from_utf16` and rejects ill-formed sequences. [OBSERVED: src/attr_io.rs:109-127]

## $STANDARD_INFORMATION (0x10) {#std-info}

The first attribute of every base record is `$STANDARD_INFORMATION` (often
abbreviated `$SI`). It is always resident, always unnamed, always exactly
one of two sizes:

- **48 bytes** — NTFS 1.x / classic form
- **72 bytes** — NTFS 3.x extended form (adds a 24-byte extension)

[OBSERVED: docs/chkdsk-improvement-findings.md §2.3.1]

Both forms are legal on NTFS 3.x volumes; an implementation chooses per
record. Microsoft `format.com` writes the **48-byte form on system records**
(slots 0..11) and the 72-byte form on user files. The extension fields claim
foreign-key references into `$Quota` and `$Secure`, which system records do
not participate in. [OBSERVED: docs/chkdsk-improvement-findings.md §2.3.1]

### Layout

```
$STANDARD_INFORMATION (NTFS 1.x, 48 bytes — also the first 48 bytes of the 3.x form):
  0x00..0x07  CreationTime         (FILETIME, 100-ns ticks since 1601-01-01 UTC)
  0x08..0x0F  LastModificationTime (FILETIME)
  0x10..0x17  LastChangeTime       ("MFT change" — bumped on attribute edits)
  0x18..0x1F  LastAccessTime       (FILETIME)
  0x20..0x23  FileAttributes       (DOS attributes + NTFS extensions)
  0x24..0x27  MaximumVersions      (typically 0)
  0x28..0x2B  VersionNumber        (typically 0)
  0x2C..0x2F  ClassId              (typically 0)

$STANDARD_INFORMATION (NTFS 3.x extended, 72 bytes):
  …same first 48 bytes…
  0x30..0x33  OwnerId              (foreign key into $Quota:$Q)
  0x34..0x37  SecurityId           (foreign key into $Secure:$SII)
  0x38..0x3F  QuotaCharged
  0x40..0x47  USN                  (Update Sequence Number for $UsnJrnl)
```

[OBSERVED: docs/chkdsk-improvement-findings.md §2.3.1]
[OBSERVED: src/record_build.rs:630-661]

### Timestamps

All four timestamps are 64-bit unsigned little-endian Windows FILETIME values
— 100-nanosecond intervals since `1601-01-01 00:00:00 UTC`. `rust-fs-ntfs`
emits this with:

```rust
const EPOCH_DIFF: u64 = 11_644_473_600;
let secs_since_1601 = unix_secs + EPOCH_DIFF;
let filetime = secs_since_1601 * 10_000_000 + (subsec_nanos / 100) as u64;
```

[OBSERVED: src/record_build.rs:22-29]

A separate `$FILE_NAME` (type 0x30) carries a parallel set of four timestamps
in the directory-index entry. `$STANDARD_INFORMATION`'s timestamps are the
authoritative ones; the `$FILE_NAME` copies are updated lazily by Windows.
[UNVERIFIED] — we have not found a permitted source explicitly stating which
copy "wins" on conflict, only the operational observation that
`fs_ntfs_set_times` updates SI but not the parent-index `$FILE_NAME` and
"matches Windows" per `docs/STATUS.md`.

### File attributes (`+0x20..+0x24`)

A 32-bit bitfield combining DOS-era flags with NTFS extensions:

| Bit          | Name                            |
| ------------ | ------------------------------- |
| `0x00000001` | Read-only                       |
| `0x00000002` | Hidden                          |
| `0x00000004` | System                          |
| `0x00000020` | Archive                         |
| `0x10000000` | Directory (NTFS-internal hint)  |

[OBSERVED: src/record_build.rs:658, 702]

System records (slots 0..11) carry exactly `0x00000006` (`HIDDEN | SYSTEM`)
on a Microsoft-formatted reference; the `ARCHIVE` bit is omitted. Setting
`ARCHIVE` on system records produces a structural divergence from
`format.com`'s output (cosmetic, but flagged by per-record byte diff).
[OBSERVED: docs/chkdsk-improvement-findings.md §2.3.2]

### Version differences

| Field group                 | NTFS 1.2 | NTFS 3.0 | NTFS 3.1 |
| --------------------------- |:--------:|:--------:|:--------:|
| 48-byte core (timestamps + FA) |    ✅   |    ✅    |    ✅    |
| 24-byte extension at `+0x30`   |    ❌   | optional | optional |
| Use of `OwnerId` / `SecurityId` |   ❌   |    ✅    |    ✅    |

[OBSERVED: docs/chkdsk-improvement-findings.md §2.3.1]


## $FILE_NAME (0x30) {#file-name-brief}

`$FILE_NAME` records the file's name, the file reference of its parent
directory, a parallel set of four timestamps, allocated/real sizes, and a
"namespace" byte (Win32, DOS, Win32+DOS, POSIX). It is always resident,
always indexed (the `indexed_flag` at attribute header `+0x16` is `1`),
and a single record may carry multiple `$FILE_NAME` attributes — one per
hard link, plus one extra Win32 alias when the file has a separate DOS 8.3
name. [OBSERVED: src/record_build.rs:664-712]
[OBSERVED: docs/mkfs-bug-catalog.md Bug 1]

The on-disk `$FILE_NAME` body holds:

```
0x00..0x07  ParentDirectoryReference  (8-byte file reference: 48-bit rec + 16-bit seq)
0x08..0x0F  CreationTime              (FILETIME)
0x10..0x17  LastModificationTime
0x18..0x1F  LastChangeTime
0x20..0x27  LastAccessTime
0x28..0x2F  AllocatedSize             (mirror of $DATA's allocated_size)
0x30..0x37  RealSize                  (mirror of $DATA's data_length)
0x38..0x3B  FileAttributes
0x3C..0x3F  EA / reparse field        (overloaded: EA size or reparse tag)
0x40        NameLength                (UTF-16 code units, max 255)
0x41        Namespace                 (0=POSIX, 1=Win32, 2=DOS, 3=Win32+DOS)
0x42..      Name                      (UTF-16 LE)
```

[OBSERVED: src/record_build.rs:694-710]

Full namespace rules — POSIX folding, Win32 / DOS pairing, and the 8.3
collision-resolution algorithm — belong to
[§4 Indexes & directories](04-indexes-directories.md#filename-namespace).
This page covers only the fact that the attribute exists, is always resident,
and that the `indexed_flag` and the `AllocatedSize` / `RealSize` mirror fields
must be set correctly to match the underlying `$DATA`'s sizes.
[OBSERVED: docs/mkfs-bug-catalog.md Bug 1]

`rust-fs-ntfs`'s `record_build` synthesises `$FILE_NAME` with namespace `3`
(Win32+DOS) for new files: a single attribute serves as both the long and
short name when those happen to coincide. [OBSERVED: src/record_build.rs:64-66]

## $ATTRIBUTE_LIST (0x20) {#attribute-list}

When a file's attributes do not fit in a single MFT record — typically because
its `$DATA` runs become too large to encode resident, or because it has many
named alternate streams, many `$FILE_NAME` hard links, or all of the above —
NTFS spills the overflow into one or more **extension records** and adds an
`$ATTRIBUTE_LIST` (type `0x20`) attribute to the **base record**.

`$ATTRIBUTE_LIST`'s value is a flat array of entries, one per logical
attribute belonging to the file (whether the attribute lives in the base
record or an extension record). Each entry carries:

- attribute type code
- name length / offset (for named attributes)
- starting VCN (for non-resident attributes spanning multiple records)
- the file reference of the MFT record that physically holds this attribute
  fragment
- the attribute instance ID

[UNVERIFIED] — exact byte layout of `$ATTRIBUTE_LIST` entries is not
reproduced here; the on-disk record format is documented in `[MS-FSCC]`.

### When it appears

A base record gains an `$ATTRIBUTE_LIST` whenever the bytes-used would
otherwise exceed `bytes_allocated`. The triggering attribute is moved out
into a fresh extension record (allocated from `$MFT:$Bitmap`) whose own
header carries a non-zero **base file reference** at `+0x20`, pointing back
at the base record. [UNVERIFIED]
[UNVERIFIED] — the choice of which attribute migrates first (large `$DATA`
runs typically; less commonly, named alternate streams) is implementation-defined
and not documented in our permitted sources.

### Base / extension relationship

- The **base record** is the canonical record for the file. Its file
  reference is what `$FILE_NAME` parent links and `$INDEX_ROOT` entries point
  at. Its `base_file_reference` field at `+0x20` is **0**.
  [OBSERVED: src/record_build.rs:289]
- An **extension record** has `base_file_reference` set to the base record's
  reference. Its own `$FILE_NAME` is absent (it is invisible to directory
  enumeration); its only purpose is to host attribute fragments listed by
  the base's `$ATTRIBUTE_LIST`.
  [UNVERIFIED]
- The base record's `$ATTRIBUTE_LIST` is the authoritative manifest. To
  fully assemble a file's attributes, an implementation reads the base,
  parses `$ATTRIBUTE_LIST`, then for each entry reads the cited extension
  record (or the base, if `mft_ref` points at the base itself).
  [UNVERIFIED]

### Traversal rules

Chain/tree walkers should use iterative traversal with an explicit
heap-allocated stack (e.g., a growable `uint32_t` array of pending MFT
record IDs) rather than recursion. [UNVERIFIED]

Three defensive checks are required:

1. **Circular reference protection.** Track visited MFT record IDs in a
   bitset; a back-reference to an already-visited record breaks the chain
   for that branch. [UNVERIFIED]
2. **Maximum entry limit.** Hard cap (e.g., 256 or 1024) on collected
   attribute fragments to bound corrupt or adversarial chains.
   [UNVERIFIED]
3. **Iterative walker.** A recursive walker on a deep chain (~25,000 records)
   exhausts the typical 8 MiB Linux stack and segfaults; the iterative form
   is bounded only by heap. [UNVERIFIED]

The reference pseudocode (`rebuild_attribute_list`):

```
stack   = [base_record_id]
visited = BitSet(mft.total_records)
while stack is not empty:
    current = stack.pop()
    if visited[current] OR collected.len > MAX:
        continue
    visited.set(current)
    for attr in record(current).attributes:
        if attr.type == 0xFFFFFFFF: break
        if attr.is_valid_data_runs:
            collected.push(AttrListEntry(type, name, vcn, mft_ref, attr_id))
        if attr.type == ATTRIBUTE_LIST_TYPE:
            for entry in parse_attrlist_entries(attr):
                child = entry.mft_reference & 0xFFFF_FFFF_FFFF
                if child != base_record_id:
                    stack.push(child)
collected.sort(key=(type, vcn))
collected = deduplicate(collected)
```

[UNVERIFIED]

### Sort and deduplication

The repair-time rebuild sorts the collected entries by `(type_code, starting_vcn)`
and removes duplicates. Out-of-order entries on a healthy volume are not
defined as corruption per se, but the rebuild emits them sorted and
expects readers to tolerate either ordering. [UNVERIFIED]
[UNVERIFIED] — we have no permitted source that pins down whether NTFS
*requires* `$ATTRIBUTE_LIST` to be sorted on a healthy mounted volume, only
that the repair pipeline emits it sorted.

`rust-fs-ntfs` does not currently emit `$ATTRIBUTE_LIST`; all base records it
synthesises are small enough to be self-contained. The read path tolerates
extension records via the upstream `ntfs` crate. [OBSERVED: docs/STATUS.md]

## Sequence numbers and reuse {#sequence-numbers}

The 16-bit sequence number at record offset `0x10` is the second half of the
64-bit "file reference" used by every cross-record pointer. Its purpose is
to detect stale references after a slot is reallocated.

### Combine to file reference

```rust
file_reference = (record_number & 0x0000_FFFF_FFFF_FFFF) | ((sequence as u64) << 48)
```

[OBSERVED: src/record_build.rs:17-19]

A `$FILE_NAME.parent_reference`, a directory `INDEX_ENTRY.file_reference`,
and a base record's `base_file_reference` all carry both halves. To resolve
a reference, an implementation reads the cited record, verifies it is in
use, **and** verifies its current sequence number matches the high 16 bits
of the reference. [UNVERIFIED]
[UNVERIFIED] — the precise validation rule (mismatch ⇒ stale ⇒ ignore vs
mismatch ⇒ corruption) is implementation-defined; we describe the
operational meaning, not a hard format invariant.

### What counts as "in use"

A record is in use iff:

1. The `IN_USE` bit (`0x0001`) at header offset `0x16` is set.
2. The corresponding bit in `$MFT:$Bitmap` is set.

The two are expected to agree; disagreement is a known repair case.
[UNVERIFIED]
[OBSERVED: src/mft_io.rs:315-319] — `rust-fs-ntfs` uses the in-record flag
on the read-modify-write hot path; the bitmap is consulted at allocation
time. [OBSERVED: src/mft_bitmap.rs]

### When sequence numbers bump

The conventional behaviour:

- A fresh record (slot allocated for the first time) gets sequence `1`.
- Each time the slot is freed and re-allocated to a new logical file, the
  sequence is incremented (mod 2^16).
- Sequence `0` is avoided in some implementations (treated as "uninitialised").
  [UNVERIFIED] — analogous to the USN-skipping-zero rule, but for sequence
  numbers we have no permitted-source statement; `rust-fs-ntfs`'s
  fresh-record path emits `1` rather than `0` but does not bump on reuse
  in the current implementation.

For the **system records (slots 0..11)** Microsoft's `format.com` writes
the deterministic value `sequence = max(1, record_number)`. Slots 0 and 1
get sequence 1; slots 2..11 get their slot number. So the root directory
at slot 5 has `sequence = 5`, and every system record's
`$FILE_NAME.parent_reference = (5, 5)` resolves cleanly without
post-format bookkeeping. [OBSERVED: docs/chkdsk-improvement-findings.md §2.2.2]
[OBSERVED: docs/mkfs-bug-catalog.md Bug 3]

User-allocated records (slot ≥ 16) use the conventional "sequence starts at
1, bumps on reuse" rule. [OBSERVED: src/record_build.rs:278]
[UNVERIFIED] — the precise initial value (1 vs slot number vs random) for
non-system records is implementation-defined; we adopt the operational
convention seen in our own writer.

### chkdsk validation

chkdsk reports `Incorrect information was detected in file record segment N`
when the system-record sequence numbers do not match the
`max(1, record_number)` pattern. A child claiming `parent_ref = (5, 5)`
fails to resolve if record 5's sequence is not `5`.
[OBSERVED: docs/chkdsk-improvement-findings.md §2.2.2]

## Attribute IDs and instance numbers {#attribute-ids}

Within a single MFT record (or, more precisely, within a base record + its
extensions taken together), every attribute carries a 16-bit **attribute
instance ID** at attribute-header offset `+0x0E`. The IDs need not be
contiguous and need not start at `0`, but each ID must appear at most once
across all attributes belonging to the file.
[OBSERVED: src/record_build.rs:130-131, 290-291]

The base record's header at `+0x28` carries `next_attribute_id`, the
smallest unused ID. Adding a new attribute uses `next_attribute_id` as the
new attribute's ID and bumps the field by 1 (mod 2^16). On a freshly built
record `rust-fs-ntfs` uses IDs `0`, `1`, `2` for `$STANDARD_INFORMATION`,
`$FILE_NAME`, `$DATA` and writes `next_attr_id = 3`. Directory records
follow the same scheme with `$INDEX_ROOT` taking ID `2`.
[OBSERVED: src/record_build.rs:130, 291]

### Uniqueness scope

The "no duplicate ID" rule applies across the **logical file** (base record
plus every extension record listed in `$ATTRIBUTE_LIST`), not just within a
single physical record. So if the base record carries an attribute with ID
`5`, no extension record may also carry an attribute with ID `5`.
[UNVERIFIED] — the precise scope of uniqueness (per-record vs per-file) is
not stated explicitly in our permitted sources; we infer it from the
deduplication key used in chain-rebuild pseudocode.

### Why instance IDs matter

A file may have multiple attributes of the same type — e.g. multiple
`$FILE_NAME` attributes (one per hard link), or multiple `$DATA` attributes
(unnamed plus alternate streams). The attribute instance ID disambiguates
them in `$ATTRIBUTE_LIST` references and in `$LogFile` log records that
target a specific attribute. [UNVERIFIED]

## Attribute compaction & migration {#attr-compaction}

This subsection is about **layout compaction**, distinct from the resident /
non-resident migration covered in [§2.6](#resident-nonresident).

Two related operations:

1. **Hole removal within a record.** When an attribute is deleted or shrunk,
   its bytes are removed and any subsequent attributes shift back so the
   end-of-attributes sentinel stays packed against the live attributes.
   `rust-fs-ntfs`'s `attr_resize::resize_resident_value` does this with
   `copy_within` on the post-fixup buffer, then updates the record's
   `bytes_used`. [OBSERVED: src/attr_resize.rs:97-101]

2. **Extension-record collapse.** When the base record's `$ATTRIBUTE_LIST`
   shrinks enough that the spilled attributes would now fit back in the
   base record, an implementation could in principle migrate the
   attributes back, drop the `$ATTRIBUTE_LIST`, and free the extension
   record's MFT slot. The format itself permits this; ordinary mounted
   drivers may do it. [UNVERIFIED]

`rust-fs-ntfs` does not currently implement either form of compaction at the
extension-record level; its `attr_resize` operates on a single record and
will not initiate a base ↔ extension migration. [OBSERVED: src/attr_resize.rs]
[OBSERVED: docs/STATUS.md]

### When the question arises in practice

- Truncating a large `$DATA` to zero on a file that has an
  `$ATTRIBUTE_LIST` — the spilled `$DATA` runs go away; whether the
  `$ATTRIBUTE_LIST` itself can be dropped depends on whether other
  attributes still spill.
- Deleting alternate data streams on a heavily-streamed file — same
  question.
- `chkdsk /F` finding a base record that *could* hold all attributes back
  but currently has an `$ATTRIBUTE_LIST`.

[UNVERIFIED] — whether `chkdsk /F` itself ever performs this collapse is
not documented in our permitted sources.

## References

- `[MS-FSCC]` File System Control Codes — Microsoft Open Specifications. See [notes/references.md](../notes/references.md).
- `rust-fs-ntfs` source: [src/mft_io.rs](../../../src/mft_io.rs),
  [src/mft_bitmap.rs](../../../src/mft_bitmap.rs),
  [src/record_build.rs](../../../src/record_build.rs),
  [src/attr_io.rs](../../../src/attr_io.rs),
  [src/attr_resize.rs](../../../src/attr_resize.rs).
- `rust-fs-ntfs` operational notes:
  [docs/STATUS.md](../../STATUS.md),
  [docs/chkdsk-debugging.md](../../chkdsk-debugging.md),
  [docs/chkdsk-improvement-findings.md](../../chkdsk-improvement-findings.md),
  [docs/mkfs-bug-catalog.md](../../mkfs-bug-catalog.md).

## Open questions

Section-local `[UNVERIFIED]` items mirrored into [notes/open-questions.md](../notes/open-questions.md):

- [ ] `IN_USE` flag and `$MFT:$Bitmap` agreement on a healthy mounted
  volume — no permitted source states which is authoritative.
- [ ] LSN at record offset `0x08` — the 0-init convention for freshly built
  records is local to `rust-fs-ntfs`; not corroborated by a permitted
  source as a format invariant.
- [ ] MFT record number at `0x2C` — we treat as NTFS-3.1-only by convention,
  but no permitted source explicitly marks the field as version-gated.
- [ ] CRC32 footer (last 4 bytes of NTFS 3.1+ MFT records) —
  post-fixup computation is asserted in lead material; `rust-fs-ntfs`
  does not yet validate or emit it, so the order-of-operations rule is
  unverified by our test suite.
- [ ] `$MFTMirr` divergence semantics (decision matrix) — uncorroborated;
  not exercised by any test in `rust-fs-ntfs`.
- [ ] Slot 9 ownership (`$Secure` vs `$Quota`) — convention-dependent across
  NTFS-3.0 vs modern Microsoft `format.com`; no canonical statement found.
- [ ] `$STANDARD_INFORMATION` timestamp authority — which copy (SI vs the
  parent-index `$FILE_NAME`) wins on conflict, beyond the operational
  observation that Windows updates SI eagerly and FN lazily.
- [ ] `$ATTRIBUTE_LIST` on-disk entry layout — traversal contract is known
  but the byte format has not been reproduced from a permitted source
  (lives in `[MS-FSCC]`).
- [ ] `$ATTRIBUTE_LIST` migration trigger — which attribute is moved out
  first when a base record overflows is implementation-defined.
- [ ] `$ATTRIBUTE_LIST` sort invariant on healthy volumes — no permitted
  source pins whether NTFS *requires* sort.
- [ ] Sequence-number reuse rule — initial value and increment-on-reuse for
  non-system records is implementation-defined; we adopt our writer's
  convention.
- [ ] File-reference resolution policy — sequence mismatch as "stale" vs
  "corrupt" is implementation-defined.
- [ ] Attribute-instance-ID uniqueness scope — per-record vs per-file
  (across base + extensions) is inferred from a deduplication-key
  argument, not stated by any permitted source.
- [ ] Sequence-number-zero avoidance — analogous to USN-skip-zero, but no
  permitted source corroborates it for sequence numbers.
- [ ] Whether `chkdsk /F` ever performs base ↔ extension attribute compaction
  — behaviour of the closed-source utility is not documented in our
  sources.

[← Prev: Volume geometry & boot sector](01-geometry-boot.md) | [TOC](../ntfs-specification.md) | [Next: Data runs & cluster allocation →](03-data-runs-bitmap.md)
