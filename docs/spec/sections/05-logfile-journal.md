[← Prev: Indexes & directories](04-indexes-directories.md) | [TOC](../ntfs-specification.md) | [Next: Special streams →](06-special-streams.md)

# 5. $LogFile & journal

## Overview {#overview}

`$LogFile` is system file at MFT record number `2`. It backs NTFS's
write-ahead log: every metadata transaction (MFT record edit, index
mutation, bitmap flip, attribute resize) is journaled here before the
target structure on disk is updated. The journal exists so a hard
power loss during a metadata transaction can be unwound or rolled
forward when the volume is next mounted, leaving on-disk metadata
self-consistent. `[UNVERIFIED]`

NTFS layers two abstractions inside `$LogFile`:

- **LFS — the Log File Service.** Generic page-structured cyclic log.
  LFS owns the restart pages, the page-level record envelope (`RCRD`
  pages), and the LSN (Log Sequence Number) addressing scheme. It is
  not NTFS-aware; it stores opaque "client data" payloads on behalf of
  registered clients. `[UNVERIFIED]`
- **NTFS as the LFS client.** NTFS is the (typically only) registered
  LFS client; client name `"NTFS"`. NTFS-specific opcode dispatch,
  redo/undo payload format, and target-attribute resolution live above
  the LFS layer. `[UNVERIFIED]`

Why this matters for crash recovery: a clean unmount writes a final
restart area with the `CLEAN_DISMOUNT` flag set; on next mount the
driver reads the restart area, sees the flag, and skips replay. A
dirty unmount leaves the flag clear; the driver then walks RCRD pages
from the oldest still-relevant LSN forward, replays committed
transactions (redo), and discards or rolls back uncommitted ones.
`[UNVERIFIED]`

`rust-fs-ntfs` does not implement journal replay. Its `mkfs` writes a
canonical "empty but valid" `$LogFile` that signals
`CLEAN_DISMOUNT = set` so Windows' `ntfs.sys` mounts the volume
without trying to replay anything. Recovery from a dirty volume is
delegated to Windows itself. `[OBSERVED: src/mkfs.rs]`
`[OBSERVED: src/fsck.rs]`

This section covers:

- LFS-level structures: restart pages, restart area, client records,
  RCRD record pages, LSN encoding.
- NTFS-client-level structures: log record header, redo/undo opcodes,
  open-attribute and dirty-page tables.
- WAL-style recovery flow: analysis, redo, undo passes.
- The shape of the canonical empty log we write at format time.
- `$UsnJrnl`: the user-facing change journal, which lives at
  `$Extend\$UsnJrnl` and is structurally unrelated to `$LogFile`.
  `[UNVERIFIED]`

Cross-references:

- Multi-sector USA fixup applies to every restart page and RCRD page;
  see [§2 USA fixup](02-mft-records.md#usa-fixup).
- Index-level opcodes target `$INDEX_ROOT` / `$INDEX_ALLOCATION` —
  see [§4 Indexes & directories](04-indexes-directories.md).
- Compression of redo payloads via LZNT1 — see
  [§6 Special streams](06-special-streams.md).

## $LogFile sizing {#sizing}

The on-disk size of `$LogFile` is recorded redundantly:

- In MFT record 2, the `$DATA` attribute's `data_size` /
  `allocated_size`. `[UNVERIFIED]`
- Inside each restart page, `LFS_RESTART_AREA.FileSize` — `ntfs.sys`
  reads this at mount and `chkdsk` compares it against the on-disk
  allocated length. If they disagree `chkdsk` reports
  `"adjusting the size of the log file"`.
  `[OBSERVED: src/mkfs.rs:242-249]`

Microsoft's `format.com` produced `0x3B_0000` bytes (≈ 3.78 MiB) of
`$LogFile` on a 256 MiB / 4 KiB-cluster reference volume, and that is
the sizing constant `rust-fs-ntfs` mirrors.
`[OBSERVED: test-diagnostics/run-20260502-154836/mac-format-label-empty]`
For other cluster sizes the value rounds up to the nearest cluster.
`[OBSERVED: src/mkfs.rs:254-255]`

Typical `$LogFile` sizes on Microsoft-formatted volumes scale with
volume size — small volumes get a few MiB, large volumes can run to
tens of MiB. `[UNVERIFIED]` Concrete scaling formula and
upper cap have not been corroborated against `[MS-NTFS]` here.

### Canonical 12 KiB shape

`rust-fs-ntfs` ships a 12 288-byte (12 KiB) prebaked blob at
`src/logfile-canonical-12k.bin`, captured from a Microsoft `format.com`
reference run and embedded into `mkfs` via `include_bytes!`.
`[OBSERVED: src/mkfs.rs:127-151]`

Layout of the 12 KiB:

| Page | Offset    | Size     | Content                                                                                                                                              |
| ---- | --------- | -------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| 0    | `0x0000`  | 4 KiB    | LFS restart page (`RSTR` magic, USA-protected). Restart area at offset `0x30`. `current_lsn = 0x104408`. Single client `"NTFS"` at offset `0x90`.    |
| 1    | `0x1000`  | 4 KiB    | Paired LFS restart page (`RSTR` magic). Slightly newer `current_lsn = 0x10634B` — `ntfs.sys` picks the higher LSN as authoritative.                  |
| 2    | `0x2000`  | 4 KiB    | Single sentinel `RCRD` page. USA at offset `0x28`. `lsn` matches the active restart's `current_lsn`.                                                  |

`[OBSERVED: src/mkfs.rs:134-146]`

Past offset `0x3000`, the reference dump is all `0xFF`. `mkfs` writes
the canonical 12 KiB at the start of `$LogFile`'s on-disk extent, then
fills the remainder of the allocated `0x3B_0000` bytes with `0xFF`.
`[OBSERVED: src/mkfs.rs:332-348]` SHA-256 of the prebaked blob:
`0a1d770715ee987934fcdfd6691507c96912b708d79b1bb8e1ce9408ce2ae368`.
`[OBSERVED: src/mkfs.rs:131-132]`

For the `fsck` recovery path (`src/fsck.rs::reset_logfile`), the
`$LogFile` `$DATA` extent is overwritten end-to-end with `0xFF`. This
is the format-level "empty log" sentinel — `ntfs.sys` treats an
all-`0xFF` log as uninitialized and reinitializes it on next mount.
`[OBSERVED: src/fsck.rs:55-62]` `[OBSERVED: src/fsck.rs:232-342]`

## Restart area {#restart-area}

The first two LFS pages of `$LogFile` are restart pages, located at:

- offset `0` (page 0)
- offset `SystemPageSize` (page 1, typically `0x1000`)

Both pages are written redundantly. The driver reads both, validates
each, and selects the more recent one. `[UNVERIFIED]`

### Selection rules

1. Read both restart pages.
2. Validate `RSTR` magic and apply USA fixup
   (see [§2 USA fixup](02-mft-records.md#usa-fixup)) on each.
3. Both valid → use the page with the higher
   `LFS_RESTART_AREA.CurrentLsn` (it is the more recent write).
4. One valid → use that one.
5. Neither valid → `$LogFile` is unrecoverable. `[UNVERIFIED]`

The double-buffer scheme exists because the restart area is written
in place; if a crash happens mid-write of one copy, the other is
still consistent. `[UNVERIFIED]`

### USA fixup

Each restart page is a multi-sector record protected by an Update
Sequence Array. The USA tail-rewrite is identical to MFT and INDX
records — see [§2 USA fixup](02-mft-records.md#usa-fixup) for the
mechanism. Fields: `MULTI_SECTOR_HEADER.usa_offset` and
`usa_count` at the page header. `[UNVERIFIED]`

## LFS_RESTART_PAGE {#restart-page}

Layout of the restart page header. Located at offset `0` and at
offset `SystemPageSize` of `$LogFile`. `[UNVERIFIED]`

| Offset | Size | Field                  | Description                                                  |
| -----: | ---: | :--------------------- | :----------------------------------------------------------- |
| `0x00` |  `8` | `MULTI_SECTOR_HEADER`  | Magic = `RSTR` (`52 53 54 52`), USA offset, USA count.       |
| `0x08` |  `8` | `ChkDskLsn`            | Check-disk LSN — written by `chkdsk`.                        |
| `0x10` |  `4` | `SystemPageSize`       | Size of this restart page (typically `4096`).                |
| `0x14` |  `4` | `LogPageSize`          | Size of `RCRD` pages (typically `4096`).                     |
| `0x18` |  `2` | `RestartOffset`        | Offset to `LFS_RESTART_AREA` from start of page.             |
| `0x1A` |  `2` | `MinorVersion`         | LFS minor version (commonly `1`).                             |
| `0x1C` |  `2` | `MajorVersion`         | LFS major version (`1` for v1.0; `2` for LFS 2.0).            |
| `0x1E` | var  | `UpdateSequenceArray`  | USA tail array, length `(usa_count - 1) * 2` bytes.          |

`[UNVERIFIED]`

### Magic and version

- Magic `RSTR` (`52 53 54 52`, ASCII "RSTR"). `[UNVERIFIED]`
- `MajorVersion == 1` → LFS 1.0 (Windows XP through 8). `[UNVERIFIED]`
- `MajorVersion == 2` → LFS 2.0 (introduced Windows 8.1 / 10). LFS 2.0
  uses larger sector alignments and different field sizes; see
  [MS-FSCC] 2.1.1. `[UNVERIFIED]`
- A v1.0 parser MUST refuse to replay a v2.0 log. `[UNVERIFIED]`

### NTFS version note

The restart page layout above is correct for NTFS 3.1 (Windows XP and
later). For NTFS 1.2 volumes the field offsets fall back to
[MS-NTFS] §2.6; in practice 1.2 volumes rarely have a dirty
`$LogFile` requiring replay.

## LFS_RESTART_AREA {#restart-area-struct}

Located inside the restart page at byte offset
`LFS_RESTART_PAGE.RestartOffset`. The restart area is the recovery
anchor — every replay starts by parsing it. `[UNVERIFIED]`

| Offset | Size | Field                | Description                                                                    |
| -----: | ---: | :------------------- | :----------------------------------------------------------------------------- |
| `0x00` |  `8` | `CurrentLsn`         | LSN of the current (most recently written) log record.                         |
| `0x08` |  `2` | `LogClients`         | Number of `LFS_CLIENT_RECORD` entries in the client array.                     |
| `0x0A` |  `2` | `ClientFreeList`     | Index of first free client record (or `0xFFFF` if none).                       |
| `0x0C` |  `2` | `ClientInUseList`    | Index of first in-use client record (or `0xFFFF` if none).                     |
| `0x0E` |  `2` | `Flags`              | Bit `0x02` = `CLEAN_DISMOUNT`; volume cleanly unmounted.                       |
| `0x10` |  `4` | `SeqNumberBits`      | Number of bits used for the sequence-number portion of LSNs.                   |
| `0x14` |  `2` | `RestartAreaLength`  | Total length of the restart area including the client array.                   |
| `0x16` |  `2` | `ClientArrayOffset`  | Offset from start of `LFS_RESTART_AREA` to the first `LFS_CLIENT_RECORD`.      |
| `0x18` |  `8` | `FileSize`           | Total size of `$LogFile` in bytes (must match on-disk allocated length).       |

`[UNVERIFIED]`

### Flag semantics

- `0x02` `CLEAN_DISMOUNT` — the file system was cleanly unmounted.
  Set on graceful shutdown; cleared as soon as the first dirty
  transaction starts. If set when the volume is next mounted, the
  driver skips journal replay entirely. `[UNVERIFIED]`
- Other flag bits in this field are not enumerated in the format
  documentation we have. `[UNVERIFIED]` against `[MS-NTFS]`.

### Last-LSN data length

The restart area includes additional fields beyond the table above
(notably the length of the last LSN data and the previous client
restart info). The format documentation we have stops at `FileSize`
and does not specify the trailing fields; full field set is
`[UNVERIFIED]` against `[MS-NTFS] §2.6`.

### File-size invariant

`FileSize` MUST equal the on-disk allocated length of `$LogFile`. A
mismatch causes `chkdsk` to log
`"CHKDSK is adjusting the size of the log file"`. The
`rust-fs-ntfs` mkfs allocates exactly `0x3B_0000` bytes for
`$LogFile`'s `$DATA` extent precisely so it matches the `FileSize`
encoded in the prebaked restart pages.
`[OBSERVED: src/mkfs.rs:242-249]`

## LFS_CLIENT_RECORD {#client-record}

Located inside the restart area, starting at `ClientArrayOffset`.
NTFS is typically the only registered client. `[UNVERIFIED]`

| Offset | Size  | Field                | Description                                                              |
| -----: | ----: | :------------------- | :----------------------------------------------------------------------- |
| `0x00` |   `8` | `OldestLsn`          | LSN of the oldest uncommitted record for this client. **Replay starts here.** |
| `0x08` |   `8` | `ClientRestartLsn`   | LSN of the latest client restart record.                                  |
| `0x10` |   `2` | `PrevClient`         | Index of previous client in the in-use list (or `0xFFFF`).                |
| `0x12` |   `2` | `NextClient`         | Index of next client in the in-use list (or `0xFFFF`).                    |
| `0x14` |   `2` | `SeqNumber`          | Client sequence number.                                                   |
| `0x16` |   `6` | `Padding`            | Reserved / alignment.                                                     |
| `0x1C` |   `4` | `ClientNameLength`   | Length of `ClientName` in bytes.                                          |
| `0x20` | `128` | `ClientName`         | Client name (UTF-16LE, typically `"NTFS"`).                               |

`[UNVERIFIED]`

### Notes

- `OldestLsn` is the lower bound of replay: every record from this LSN
  forward is potentially relevant. Records older than this can be
  reclaimed by LFS. `[UNVERIFIED]`
- `ClientName` is fixed-width 128 bytes (64 UTF-16 code units),
  zero-padded after `ClientNameLength` bytes. The string is `"NTFS"`
  (4 UTF-16 code units, 8 bytes) for the NTFS client.
  `[OBSERVED: src/mkfs.rs:137]` (canonical blob places this client
  record at offset `0x90` inside page 0).
- Free / in-use linkage. `ClientFreeList` and `ClientInUseList` in
  the restart area form two singly-linked indices into the client
  array; `PrevClient` / `NextClient` thread the in-use list. With one
  client (the common case) the free list is empty (`0xFFFF`) and the
  in-use list has a single entry pointing at index `0`. `[UNVERIFIED]`

## RCRD record pages {#rcrd-page}

After the two restart pages, the body of `$LogFile` is a cyclic
sequence of `RCRD` pages. Each page is `LogPageSize` bytes (typically
4 KiB), USA-protected, and holds one or more LFS log records.
`[UNVERIFIED]`

### Multi-sector USA fixup

RCRD pages use the same USA fixup mechanism as MFT records and INDX
blocks. Apply USA fixup before parsing the page body, re-apply USA
protection before writing back. See
[§2 USA fixup](02-mft-records.md#usa-fixup). `[UNVERIFIED]`

### RCRD page header layout

| Offset  | Size | Field                  | Description                                                |
| ------: | ---: | :--------------------- | :--------------------------------------------------------- |
| `0x000` |  `4` | Magic                  | `RCRD` (`52 43 52 44`).                                    |
| `0x004` |  `2` | USA offset             | Byte offset to the USA within this page.                   |
| `0x006` |  `2` | USA count              | USA count (sector count + 1).                              |
| `0x008` |  `8` | `last_lsn`             | LSN of the last LFS record on this page (or file offset).  |
| `0x010` |  `4` | `flags`                | Page flags.                                                |
| `0x014` |  `2` | `page_count`           | Total pages this multi-page record spans.                  |
| `0x016` |  `2` | `page_position`        | Index of this page within the multi-page record.           |
| `0x018` |  `8` | `next_record_offset`   | Byte offset (within the page) where the next LFS record begins. |
| `0x020` |  `8` | `last_end_lsn`         | LSN of the last record that ends within this page.         |
| `0x028` | var  | USA fixup array        | `(USA_count - 1) * 2` bytes — see [§2 USA fixup](02-mft-records.md#usa-fixup). |

`[UNVERIFIED]`

### Records inside the page

A page may contain multiple LFS log records packed end-to-end after
the header + USA. A single log record may also span pages, indicated
by the per-record `flags & 0x01` bit. `[UNVERIFIED]`

The canonical empty `$LogFile` ships **one** RCRD page (page 2 of
the 12 KiB blob), with `lsn` matching the active restart's
`CurrentLsn`. Its purpose is to be a syntactically valid sentinel —
not to carry any pending transaction. `[OBSERVED: src/mkfs.rs:141-142]`

## Log Sequence Number (LSN) {#lsn}

LSNs are 64-bit values that totally order log records. `[UNVERIFIED]`
The encoding splits the 64 bits into two parts:

- **Sequence number** — high bits, advanced when the cyclic log
  wraps. Width is `LFS_RESTART_AREA.SeqNumberBits`. `[UNVERIFIED]`
- **File offset** — remaining low bits, byte offset into the
  cyclic log region. `[UNVERIFIED]`

Implication: when the log wraps, the file-offset bits roll over to
zero and the sequence-number bits increment by one. Comparisons
between two LSNs use unsigned 64-bit comparison after both are
extracted from the same `SeqNumberBits` configuration.
`[UNVERIFIED]` — exact bit-packing direction (high vs low, big- vs
little-endian within the 64-bit word) is not stated in the format
documentation we have; treat as `[UNVERIFIED]` against
`[MS-NTFS] §2.6`.

### Wrap-around

Because the log is cyclic, a "newer" LSN can have a smaller raw
64-bit value than an "older" one across a wrap boundary, so naïve
numeric comparison is unsafe near the wrap. The driver must compare
in two stages: first by sequence-number bits, then by file-offset
bits. `[UNVERIFIED]` against `[MS-NTFS]`.

### Selecting the active restart

The choice between the two restart pages is "higher `CurrentLsn` wins"
— this comparison must respect `SeqNumberBits` so wrap-around is
handled correctly. `[UNVERIFIED]`

## Log record header {#log-record-header}

LFS log records sit inside an RCRD page, immediately after the page
header + USA. There are two header layers: `[UNVERIFIED]`

1. **LFS log record header** — generic, owned by LFS.
2. **NTFS client data** — payload appended by the NTFS client.

### LFS log record header

| Offset  | Size | Field                  | Description                                              |
| ------: | ---: | :--------------------- | :------------------------------------------------------- |
| `+0x00` |  `8` | `this_lsn`             | LSN of this log record.                                  |
| `+0x08` |  `8` | `client_previous_lsn`  | Previous LSN for this client (forms the undo chain).     |
| `+0x10` |  `8` | `client_undo_next_lsn` | Next LSN to walk during undo.                            |
| `+0x18` |  `4` | `client_data_length`   | Length of the trailing client data (redo + undo).        |
| `+0x1C` |  `2` | `client_id.seq_number` | Client sequence number.                                  |
| `+0x1E` |  `2` | `client_id.client_index` | Client index (`0` = NTFS).                             |
| `+0x20` |  `4` | `record_type`          | `0x01` = client record, `0x02` = client restart record.  |
| `+0x24` |  `4` | `transaction_id`       | Transaction ID this record belongs to.                   |
| `+0x28` |  `2` | `flags`                | Bit `0x01` = record spans multiple pages.                |
| `+0x2A` |  `6` | `padding`              | Alignment.                                               |
| `+0x30` | var  | `client_data`          | NTFS client payload (redo / undo, see below).            |

`[UNVERIFIED]`

### Client data start offset depends on LFS version

- LFS `major_version == 1` (Windows XP / 7) → client data starts at
  offset `0x28` from the LFS record. `[UNVERIFIED]`
- LFS `major_version == 2` (Windows 8 / 10 / 11) → starts at offset
  `0x30` due to extended padding. `[UNVERIFIED]`

The structural table above shows the v2 layout. A v1 parser must
subtract the 8-byte padding gap before reading `client_data`. Failing
to adjust this offset misaligns every opcode parse downstream.
`[UNVERIFIED]`

### NTFS client data — redo/undo header

The NTFS client appends a fixed header followed by an optional LCN
list, then the redo and undo payload bytes. `[UNVERIFIED]`

| Offset  | Size  | Field                     | Description                                                              |
| ------: | ----: | :------------------------ | :----------------------------------------------------------------------- |
| `+0x00` |   `2` | `redo_operation`          | Redo opcode (e.g. `InitializeFileRecordSegment` = `0x02`).               |
| `+0x02` |   `2` | `undo_operation`          | Undo opcode.                                                             |
| `+0x04` |   `2` | `redo_offset`             | Offset within target attribute for the redo data.                        |
| `+0x06` |   `2` | `redo_length`             | Length in bytes of the redo data.                                        |
| `+0x08` |   `2` | `undo_offset`             | Offset within target attribute for the undo data.                        |
| `+0x0A` |   `2` | `undo_length`             | Length in bytes of the undo data.                                        |
| `+0x0C` |   `2` | `target_attribute`        | Target attribute type code (e.g. `0x0080` = `$DATA`).                    |
| `+0x0E` |   `2` | `lcns_to_follow`          | Number of LCN entries in `lcn_list[]`.                                   |
| `+0x10` |   `2` | `record_offset`           | Offset within the MFT record.                                            |
| `+0x12` |   `2` | `padding`                 | Often mis-cited as `attribute_offset`; use `redo_offset` for placement.  |
| `+0x14` |   `2` | `cluster_block_offset`    | Cluster offset within the target.                                        |
| `+0x18` |   `4` | `target_vcn`              | Target VCN (used for non-resident attributes).                           |
| `+0x20` | `8×N` | `lcn_list[]`              | Array of LCNs of target clusters; `N = lcns_to_follow`.                  |
| `var`   | var   | `redo_data`               | Redo payload (this is what gets copied into the target).                 |
| `var`   | var   | `undo_data`               | Undo payload.                                                            |

`[UNVERIFIED]`

### Parsing rules

1. Read the fixed header (offsets `+0x00`..`+0x1F`) to obtain
   `lcns_to_follow`. `[UNVERIFIED]`
2. If `lcns_to_follow > 0`, read `N * 8` bytes for the LCN array.
   `[UNVERIFIED]`
3. `redo_data` immediately follows the LCN array (or the fixed header
   if `lcns_to_follow == 0`). `[UNVERIFIED]`
4. `undo_data` follows `redo_data`. `[UNVERIFIED]`

The "padding often mis-cited as `attribute_offset`" warning at
`+0x12` is a known foot-gun in third-party documentation: use
`redo_offset` (not `record_offset` or `padding`) as the placement
offset within the located attribute. `[UNVERIFIED]`

### Redo-payload compression

If a log record's `redo_data` is compressed, it must be decompressed
using LZNT1 before application. See
[§6 Special streams](06-special-streams.md). `[UNVERIFIED]`

## Operation codes {#opcodes}

The full opcode table for v1.0 dispatch. `[UNVERIFIED]`

| Opcode | Name                                  | Category    | v1.0 handler            |
| -----: | :------------------------------------ | :---------- | :---------------------- |
| `0x00` | *Reserved*                            | —           | No-op                   |
| `0x01` | `Noop`                                | Control     | No-op                   |
| `0x02` | `InitializeFileRecordSegment`         | MFT         | Generic copy            |
| `0x03` | `DeallocateFileRecordSegment`         | MFT         | Generic copy            |
| `0x04` | `WriteEndOfFileRecordSegment`         | MFT         | Generic copy            |
| `0x05` | `CreateAttribute`                     | Attribute   | Generic copy            |
| `0x06` | `DeleteAttribute`                     | Attribute   | Generic copy            |
| `0x07` | `UpdateResidentValue`                 | Attribute   | Generic copy            |
| `0x08` | `UpdateNonresidentValue`              | Data        | Generic copy            |
| `0x09` | `UpdateMappingPairs`                  | Data run    | Generic copy            |
| `0x0A` | `DeleteDirtyClusters`                 | Bitmap      | Specialized             |
| `0x0B` | `SetNewAttributeSizes`                | Attribute   | Generic copy            |
| `0x0C` | `AddIndexEntryToRoot`                 | Index       | Generic copy            |
| `0x0D` | `DeleteIndexEntryFromRoot`            | Index       | Generic copy            |
| `0x0E` | `AddIndexEntryToAllocationBuffer`     | Index       | Generic copy            |
| `0x0F` | `DeleteIndexEntryFromAllocationBuffer`| Index       | Generic copy            |
| `0x10` | `WriteEndOfIndexBuffer`               | Index       | Generic copy            |
| `0x11` | `SetIndexEntryVcnInRoot`              | Index       | Generic copy            |
| `0x12` | `SetIndexEntryVcnInAllocationBuffer`  | Index       | Generic copy            |
| `0x13` | `UpdateFileNameInRoot`                | Index       | Generic copy            |
| `0x14` | `UpdateFileNameInAllocationBuffer`    | Index       | Generic copy            |
| `0x15` | `SetBitsInNonResidentBitMap`          | Bitmap      | Generic copy            |
| `0x16` | `ClearBitsInNonResidentBitMap`        | Bitmap      | Generic copy            |
| `0x17` | `HotFix`                              | Repair      | Specialized             |
| `0x18` | `EndTopLevelAction`                   | Control     | Specialized             |
| `0x19` | `PrepareTransaction`                  | Tx control  | Specialized             |
| `0x1A` | `CommitTransaction`                   | Tx control  | Specialized             |
| `0x1B` | `ForgetTransaction`                   | Tx control  | Specialized             |
| `0x1C` | `OpenNonresidentAttribute`            | Attribute   | Specialized             |
| `0x1D` | `OpenAttributeTableDump`              | Checkpoint  | No-op (restart data)    |
| `0x1E` | `AttributeNamesDump`                  | Checkpoint  | No-op (restart data)    |
| `0x1F` | `DirtyPageTableDump`                  | Checkpoint  | No-op (restart data)    |
| `0x20` | `TransactionTableDump`                | Checkpoint  | No-op (restart data)    |
| `0x21` | `UpdateRecordDataRoot`                | Index       | Generic copy            |
| `0x22` | `UpdateRecordDataAllocation`          | Index       | Generic copy            |
| `0x23` | `UpdateRelativeDataIndex`             | Index       | Generic copy            |
| `0x24` | `UpdateRelativeDataAllocation`        | Index       | Generic copy            |
| `0x25` | `ZeroEndOfFileRecord`                 | MFT         | Generic copy            |
| `0x26` | `UpdateFileNameDataRoot`              | Index       | Generic copy            |
| `0x27` | `UpdateFileNameDataAllocation`        | Index       | Generic copy            |
| `0x28` | `SetStandardInformation`              | Attribute   | Generic copy            |

`[UNVERIFIED]`

### Handler categories

The three-table dispatch model summarises 38 enumerated cases as
follows: `[UNVERIFIED]`

- **Generic positional copy (23 opcodes).** Read `redo_data` from the
  log record, copy `redo_length` bytes to `redo_offset` within the
  located target attribute. `[UNVERIFIED]`
- **No-op (9 opcodes):** `0x00`, `0x01`, `0x1D`–`0x20`. Control /
  checkpoint records that produce no on-disk modification.
  `[UNVERIFIED]`
- **Specialized (5 opcodes):** `0x0A`, `0x17`–`0x1C`. Transaction
  control and non-trivial repair operations; require per-opcode
  custom logic. `[UNVERIFIED]`

`[UNVERIFIED]` — opcodes `0x21`–`0x28` appear in the structural
table but not in the corresponding dispatch-count summary; they are
believed to be later additions, all generic copy. Treat opcode
counts past `0x20` as `[UNVERIFIED]` against `[MS-NTFS]`.

### Undo dispatch is largely disabled

The repair contract treats most undo operations as no-ops. The
engine model is **redo-only forward replay** with no rollback during
repair; uncommitted transactions are simply skipped, and their
side-effects are cleaned up by subsequent MFT / bitmap / index
verification phases. `[UNVERIFIED]`

### Opcode safety constraint

Before applying redo data, the engine MUST enforce
`target_offset_within_attr + redo_length ≤ attr_size`. Skipping this
check produces buffer overflows and silent corruption of adjacent
attributes or the record tail. `[UNVERIFIED]`

### Replay order within an MFT record

When multiple log records target the same MFT record in a single
transaction, attribute types are written in order: `$INDEX_ALLOCATION`
before `$DATA`. This preserves referential consistency between the
index and the data it points at. `[UNVERIFIED]`

## Open attribute table & dirty page table {#tracking-tables}

LFS maintains two in-memory tracking tables that get checkpointed
into the log periodically and are reconstructed at restart.
`[UNVERIFIED]`

### Open attribute table

Maps attribute identifiers used inside log records (compact integers)
to full `(MFT-record, attribute-type, attribute-name)` tuples. Log
records store the compact ID; replay resolves it through this table.
The dump opcode `0x1D OpenAttributeTableDump` carries a snapshot
into the log. The companion opcode `0x1E AttributeNamesDump` carries
the attribute-name strings referenced by the dumped table.
`[UNVERIFIED]` — exact field layout is not in the format
documentation we have and not corroborated against `[MS-NTFS]` here.

### Dirty page table

Tracks pages of MFT / index buffers that have been modified in memory
but not yet flushed to disk. For each dirty page:

- LSN of the oldest log record whose effects haven't been flushed.
- Target VCN / file location.
- Length / cluster count.

The dump opcode `0x1F DirtyPageTableDump` snapshots this table; the
restart pass uses the snapshot to determine the lower bound of the
redo pass. `[UNVERIFIED]` — field layout not in the format
documentation we have.

### Transaction table

A third table — the transaction table — tracks active transactions:
their current LSN, their state (active / prepared / committed /
forgotten), and their undo chain head. The dump opcode is
`0x20 TransactionTableDump`. The transaction table is what feeds the
`PrepareTransaction` / `CommitTransaction` / `ForgetTransaction`
state machine described in [§5 WAL crash recovery flow](#wal-recovery).
`[UNVERIFIED]`

### Use at restart

The three tables are flushed to the log at every checkpoint. On
restart, the analysis pass walks log records forward, reapplying
table dumps as it goes; the resulting table state defines what the
redo and undo passes do. `[UNVERIFIED]`

## WAL crash recovery flow {#wal-recovery}

The replay engine is structured as three passes over the log,
following standard write-ahead-log recovery:

1. **Analysis pass.**
2. **Redo pass.**
3. **Undo pass** (largely disabled in the repair model).

`[UNVERIFIED]`

### Analysis pass

Goal: reconstruct the in-memory tables (open-attribute, dirty-page,
transaction) as of the crash, and determine the LSN range that the
redo pass must cover.

Steps:

1. Locate the active restart page (higher valid `CurrentLsn`).
2. From the restart area: read `CurrentLsn`, `Flags`, `SeqNumberBits`,
   `LogClients`, `ClientArrayOffset`. `[UNVERIFIED]`
3. If `Flags & CLEAN_DISMOUNT` is set, journal is clean — abort
   replay, return. `[UNVERIFIED]`
4. From each `LFS_CLIENT_RECORD`, read `OldestLsn` — this is the
   lower bound of the analysis walk.
5. Walk RCRD pages from `OldestLsn` up to `CurrentLsn`. For each
   record:
   - Apply USA fixup. `[UNVERIFIED]`
   - Parse the LFS record header.
   - If `record_type` is a checkpoint dump (`0x1D`–`0x20`),
     reconstruct the corresponding table from the dumped payload.
   - If `record_type` is a transaction-control record:
     - `0x19 PrepareTransaction` → mark transaction as prepared
       (treated as uncommitted for redo purposes).
     - `0x1A CommitTransaction` → mark transaction as committed.
     - `0x1B ForgetTransaction` → remove the transaction from the
       table.
   - `[UNVERIFIED]`

After the analysis walk, only **committed** transactions are
candidates for redo; uncommitted ones are skipped. `[UNVERIFIED]`

### Redo pass

Goal: re-apply every committed log record's `redo_data` to the
target structure, in strict LSN order.

For each log record in a committed transaction:

1. **Pre-replay minimal validation** of the loaded target block
   (CRITICAL — runs before any structural verification phase):
   - Block MUST pass USA fixup; an `ERR_USA_MISMATCH` aborts replay
     for this record.
   - The `FILE` (MFT record) or `INDX` (index buffer) magic MUST be
     correct.
   - The block's `allocated_size` MUST be a valid value (`1024` or
     `4096`).
   - Failure of any check → silently drop the log record and abort
     replay for this entry. Applying journal edits to a fundamentally
     corrupt block writes data to wrong offsets and exacerbates
     corruption. `[UNVERIFIED]`
2. Resolve target cluster(s) via `lcn_list[]`.
3. Read the target MFT record / INDX page. Apply USA fixup.
4. Locate the target attribute by `(target_attribute, name)` using
   case-insensitive `$UpCase` comparison. `[UNVERIFIED]`
5. **Attribute not found** — create a new attribute of the specified
   type, aligned to an 8-byte boundary. `[UNVERIFIED]`
6. **Attribute found** — dispatch on `redo_operation`:
   - No-op opcodes (`0x00`, `0x01`, `0x1D`–`0x24`) — skip.
   - Generic-copy opcodes — `memcpy(attr.data + redo_offset,
     redo_data, redo_length)`. Bounds-check first. `[UNVERIFIED]`
   - Specialized opcodes — apply per-opcode logic, or in v1.0 emit
     `E_JOURNAL_SKIP` and skip rather than misapply. `[UNVERIFIED]`
7. Re-apply USA protection. Write the block back. `[UNVERIFIED]`

Replay order:

- Strict LSN order across the log. `[UNVERIFIED]`
- Within a single MFT-record update, attribute order is
  `$INDEX_ALLOCATION` before `$DATA` for referential consistency.
  `[UNVERIFIED]`

### Undo pass

In repair contexts, undo is largely disabled. The engine is
redo-only forward replay; uncommitted transactions are not rolled
back at journal-replay time, and their effects are cleaned up later
by MFT / bitmap / index verification. `[UNVERIFIED]`

A full driver (not the repair tool) does perform undo on
uncommitted transactions during normal mount, walking each
transaction's `client_previous_lsn` chain in reverse. `[UNVERIFIED]`

### Finalization

After all committed transactions have been replayed:

1. Flush all caches (MFT, `$Bitmap`, index buffers) to disk.
2. Write a fresh restart area with `Flags |= CLEAN_DISMOUNT` and
   updated `CurrentLsn`. `[UNVERIFIED]`

### Reference pseudocode

Reference pseudocode for the replay engine: `[UNVERIFIED]`

```text
function replay_journal(io, mft, logfile):
    restart = find_latest_restart_page(logfile)
    if restart.flags & CLEAN_DISMOUNT:
        return                          # journal is clean

    lsn = restart.current_lsn
    while record = logfile.read_record(lsn):
        if record.record_type != CLIENT_RECORD:
            lsn = record.next_lsn
            continue

        client = record.client_data
        redo_op       = le16(client[0x00])
        redo_offset   = le16(client[0x04])
        redo_length   = le16(client[0x06])
        target_type   = le16(client[0x0C])
        redo_data     = client[header_size : header_size + redo_length]

        if redo_op in {0x00, 0x01, 0x1D..0x24}:
            lsn = record.next_lsn       # no-op
            continue
        if redo_op in {0x0A, 0x17..0x1C}:
            warn(E_JOURNAL_SKIP, redo_op)   # specialized; v2.0
            lsn = record.next_lsn
            continue

        target = io.read_clusters(read_lcn_list(client))
        apply_usa_fixup(target, mft_record_size)
        attr = find_attribute_by_type_and_name(
            target, target_type, record.attribute_name)
        if attr is NULL:
            attr = create_attribute(target, target_type)

        memcpy(attr.data + redo_offset, redo_data, redo_length)

        apply_usa_protection(target, mft_record_size)
        io.write_clusters(target_lcns, target)

        lsn = record.next_lsn

    mft.flush()
    io.sync()
```

`[UNVERIFIED]`

### General WAL recovery shape (orthogonal to NTFS)

A generic WAL recovery loop is also used elsewhere in repair-tool
contexts (not for `$LogFile` itself, but for an internal WAL that
backs a repair tool's own writes). Its shape: `[UNVERIFIED]`

```text
for each WAL entry from start to end:
    validate self-CRC; abort on mismatch
    validate magic; abort on mismatch
    if entry.flags == COMMITTED:                   # already done
        continue
    disk = io.read(entry.disk_offset, entry.payload_len)
    if crc32(disk) == entry.old_crc32:
        io.write(entry.disk_offset, entry.payload)
        io.sync()
        entry.flags = COMMITTED
    elif crc32(disk) == entry.new_crc32:
        entry.flags = COMMITTED                    # already applied
    else:
        abort(E_WAL_CONFLICT)                      # external modification
```

`[UNVERIFIED]` This is a CRC-keyed roll-forward / no-op
pattern: each entry's old / new state is recorded so the recovery
pass can decide whether the write happened or not by hashing the
current on-disk bytes. NTFS's `$LogFile` does not use CRC keying;
it uses LSN ordering and the `CLEAN_DISMOUNT` flag.

## "Empty but valid" $LogFile {#empty-valid-shape}

The minimum shape required for Windows / `ntfs.sys` to mount cleanly
is the shape `rust-fs-ntfs` writes at format time:

| Page | Size  | Content                                                                                                        |
| ---- | ----: | :------------------------------------------------------------------------------------------------------------- |
| 0    | 4 KiB | Restart page (`RSTR`). Restart area at offset `0x30`. `Flags` includes `CLEAN_DISMOUNT`. `FileSize` matches the on-disk allocation. Single client record `"NTFS"` at offset `0x90`. |
| 1    | 4 KiB | Paired restart page (`RSTR`). Slightly newer `CurrentLsn` so `ntfs.sys` selects this one. Same `CLEAN_DISMOUNT` flag.                          |
| 2    | 4 KiB | One sentinel `RCRD` page. USA at offset `0x28`. `lsn` matches the active restart's `CurrentLsn`.                                              |
| 3..N | rest  | All `0xFF`.                                                                                                    |

`[OBSERVED: src/mkfs.rs:127-151]` `[OBSERVED: src/logfile-canonical-12k.bin]`

Why this shape works:

- `ntfs.sys` reads both restart pages, validates `RSTR` magic + USA,
  and picks the higher `CurrentLsn`. `[UNVERIFIED]`
- It checks `Flags & CLEAN_DISMOUNT`; the bit is set, so it skips
  replay. `[UNVERIFIED]`
- It compares `LFS_RESTART_AREA.FileSize` against the on-disk
  allocated length; `mkfs` allocates exactly `0x3B_0000` bytes so
  these match. `[OBSERVED: src/mkfs.rs:242-249]`
- Page 2's `RCRD` exists so `ntfs.sys`' subsequent log scan finds a
  syntactically valid page; `0xFF` past page 2 is the format-level
  "uninitialized log" sentinel. `[OBSERVED: src/mkfs.rs:144-146]`

Subsequent volume mounts on Windows produce real RCRD records as
metadata operations occur; the canonical empty shape only has to
survive the very first mount.

### `fsck::reset_logfile` shape

When `rust-fs-ntfs::fsck::reset_logfile` is invoked on an existing
volume, it overwrites the entire `$LogFile $DATA` extent with
`0xFF`. `ntfs.sys` treats this as an uninitialized log and writes
fresh restart + RCRD pages on next mount; combined with clearing
the volume dirty flag (`fsck::clear_dirty`), this is the weakest
recovery the library offers — no replay, just "force the driver to
start over". `[OBSERVED: src/fsck.rs:55-62]`
`[OBSERVED: src/fsck.rs:232-342]`

## $UsnJrnl {#usn-jrnl}

`$UsnJrnl` (Update Sequence Number Journal) is **fundamentally
distinct** from `$LogFile`. `[UNVERIFIED]`

| Attribute    | `$LogFile`                                  | `$UsnJrnl`                                   |
| :----------- | :------------------------------------------ | :------------------------------------------- |
| Location     | MFT record `2` (top of MFT).                | `\$Extend\$UsnJrnl` (system files subtree).  |
| Purpose      | Transactional redo/undo journal.            | User-facing audit trail of FS operations.    |
| Content      | Raw before/after attribute payloads.        | High-level metadata (create, rename, delete). |
| Required for mount? | Yes — driver consults restart area. | No — optional feature.                       |
| Repair use   | Structural recovery via replay.             | Advisory only (filename hints, last-resort). |

`[UNVERIFIED]`

### Streams

`$UsnJrnl` lives in MFT record `\$Extend\$UsnJrnl`. It carries two
named `$DATA` streams: `[UNVERIFIED]`

- **`$Max`** — fixed-size header describing the journal:
  current journal ID, maximum size, allocation delta, etc.
  `[UNVERIFIED]` — exact `$Max` field layout is not in the format
  documentation we have and not corroborated against `[MS-NTFS]` here.
- **`$J`** — the journal data itself. Sparse non-resident stream;
  records are `USN_RECORD_V2` entries packed sequentially. Older
  entries beyond the configured maximum size are released (the
  stream's start range is "punched" sparse) so the on-disk allocated
  length stays bounded even though USN values monotonically increase.
  `[UNVERIFIED]` against `[MS-NTFS]` for the punching
  mechanism.

### Disabled / absent semantics

A volume MAY have `$UsnJrnl` disabled. The repair contract:

1. Verify the `\$Extend\$UsnJrnl` record exists.
2. If it does not exist, or `$J`'s size is zero → journal disabled or
   empty → silently skip the advisory scan (no error). `[UNVERIFIED]`

A failure during `$UsnJrnl` parsing MUST NEVER cause repair to abort:
the entire `$UsnJrnl` advisory path is optional. Any error during
scanning falls back to generic naming. `[UNVERIFIED]`

### Validation

Before trusting a record:

- Validate `MajorVersion`, `MinorVersion`, `RecordLength`. Malformed
  records are silently skipped. `[UNVERIFIED]`
- **Vulnerability fix (CRITICAL):** explicitly reject
  `RecordLength > 65 536` and `FileNameLength > 1024`. A malicious
  volume can supply `0xFFFF` for these lengths and trigger a buffer
  overflow during filename recovery. `[UNVERIFIED]`

### Staleness rules

A `USN_RECORD` is stale if:

- `Usn` is older than the highest USN tracked in `$MFT_BITMAP` or the
  `$J` stream header. `[UNVERIFIED]`
- `TimeStamp` is older than the MFT record's
  `$STANDARD_INFORMATION.LastModificationTime`. `[UNVERIFIED]`

Stale records are advisory only — usable as filename hints for
orphan recovery, never as ground truth for structural repair.
`[UNVERIFIED]`

### Failure modes

| Failure                                          | Engine behavior                                |
| :----------------------------------------------- | :--------------------------------------------- |
| Journal disabled (`$J` size = 0)                 | Skip silently; use `FILE####.CHK` fallback.    |
| Journal fully wrapped (all records stale)        | Skip; staleness check rejects every entry.     |
| `FileReferenceNumber` collision (reused MFT slot) | Mitigated by `SequenceNumber` cross-check.     |
| `$UsnJrnl` data runs corrupt                     | Treat as disabled; skip without error.         |
| Multiple USN records for one MFT ID              | Use the highest `Usn` value (most recent op).  |

`[UNVERIFIED]`

## USN_RECORD_V2 layout {#usn-record-v2}

Variable-length record packed into the `$J` stream. `[UNVERIFIED]`

| Offset | Size | Field                       | Description                                                  |
| -----: | ---: | :-------------------------- | :----------------------------------------------------------- |
| `0x00` |  `4` | `RecordLength`              | Total record size in bytes (variable).                       |
| `0x04` |  `2` | `MajorVersion`              | MUST be `2`.                                                  |
| `0x06` |  `2` | `MinorVersion`              | MUST be `0`.                                                  |
| `0x08` |  `8` | `FileReferenceNumber`       | MFT reference: 48-bit record index + 16-bit sequence number. |
| `0x10` |  `8` | `ParentFileReferenceNumber` | MFT reference of parent directory.                           |
| `0x18` |  `8` | `Usn`                       | 64-bit Update Sequence Number.                                |
| `0x20` |  `8` | `TimeStamp`                 | FILETIME (100-ns intervals since 1601-01-01 UTC).            |
| `0x28` |  `4` | `Reason`                    | Change-reason bit flags.                                      |
| `0x2C` |  `4` | `SourceInfo`                | Source-information flags.                                     |
| `0x30` |  `4` | `SecurityId`                | Security descriptor ID (index into `$Secure:$SII`).           |
| `0x34` |  `4` | `FileAttributes`             | Standard Win32 file attributes.                               |
| `0x38` |  `2` | `FileNameLength`            | Length of `FileName` in bytes.                                 |
| `0x3A` |  `2` | `FileNameOffset`            | Offset to `FileName` from record start (MUST be `0x3C` for V2). |
| `0x3C` | var  | `FileName`                  | UTF-16LE; **not** null-terminated.                            |

`[UNVERIFIED]`

### Validation rules

- `MajorVersion` MUST be `2`. Other versions are skipped. `[UNVERIFIED]`
- `RecordLength` MUST satisfy `0x3C ≤ RecordLength ≤ 0xFFFF`.
  `[UNVERIFIED]`
- `FileNameOffset` MUST be `0x3C` for V2. `[UNVERIFIED]`
- `FileNameLength` MUST be `≤ RecordLength - FileNameOffset`.
  `[UNVERIFIED]`

### `FileReferenceNumber` decomposition

64-bit value, packed as: `[UNVERIFIED]`

- Lower 48 bits: MFT record index.
- Upper 16 bits: sequence number.

Cross-check the sequence number against the actual MFT record's
sequence number to detect stale records (the slot may have been
recycled to a different file). `[UNVERIFIED]`

### `Reason` flag values

The `Reason` field is a bitmask of change reasons. The exact flag
constants (`USN_REASON_DATA_OVERWRITE`, `USN_REASON_FILE_CREATE`,
`USN_REASON_RENAME_OLD_NAME`, etc.) are defined by `[MS-FSCC]`.
`[UNVERIFIED]` — the values are not enumerated here; treat the
constant table as `[UNVERIFIED]` against `[MS-FSCC §2.3.6]`.

### Newer versions

Modern Windows can also emit `USN_RECORD_V3` (extends the FRN to
128-bit ReFS-style references) and `USN_RECORD_V4` (range tracking
for individual write operations). Both are out of scope for V2-only
parsing in the repair contract. `[UNVERIFIED]` — V3 / V4 field
layouts are not in the format documentation we have.

## References

- `[OBSERVED: src/mkfs.rs]` — canonical `$LogFile` blob baked at
  format time.
- `[OBSERVED: src/logfile-canonical-12k.bin]` — 12 288-byte prebaked
  blob, SHA-256 `0a1d770715ee987934fcdfd6691507c96912b708d79b1bb8e1ce9408ce2ae368`.
- `[OBSERVED: src/fsck.rs]` — `reset_logfile` recovery path.
- `[OBSERVED: test-diagnostics/run-20260502-154836/mac-format-label-empty]`
  — Microsoft `format.com` reference run providing the canonical
  bytes.
- `[MS-NTFS]`, `[MS-FSCC]` — Microsoft Open Specifications, cited
  per-claim above for `[UNVERIFIED]` items pending corroboration.

## Open questions

`[UNVERIFIED]` claims local to this section, copied into
`notes/open-questions.md`:

- LFS restart-area bit flags beyond `0x02 CLEAN_DISMOUNT` — full
  enumeration uncorroborated.
- `LFS_RESTART_AREA` field set past `FileSize` (last-LSN data length
  and previous-client-restart info) — not in any permitted-source
  structural table we have ingested.
- LSN bit-packing direction (high vs low half of the 64-bit word) —
  no permitted source pins this.
- LSN wrap-around comparison rules — described conceptually
  elsewhere; not as a formal algorithm in any permitted source.
- Opcodes `0x21`–`0x28` — appear in our opcode table but not in any
  dispatch-count summary we can corroborate.
- Open-attribute table on-disk dump format (opcode `0x1D`).
- Dirty-page table on-disk dump format (opcode `0x1F`).
- Driver-side undo behavior on dirty mount — not pinned to a
  permitted source.
- `$UsnJrnl::$Max` exact field layout.
- `$UsnJrnl::$J` sparse-truncation mechanism for old entries.
- `USN_RECORD_V2.Reason` flag constants — defined by `[MS-FSCC]`
  but not yet enumerated in this section.
- `USN_RECORD_V3` / `V4` field layouts — pending `[MS-FSCC]`
  corroboration.
- `$LogFile` size scaling formula across volume sizes — no permitted
  source quotes a formula; only the 256 MiB / 4 KiB-cluster reference
  is concrete.

[← Prev: Indexes & directories](04-indexes-directories.md) | [TOC](../ntfs-specification.md) | [Next: Special streams →](06-special-streams.md)
