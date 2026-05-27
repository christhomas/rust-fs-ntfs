[← Prev: $LogFile & journal](05-logfile-journal.md) | [TOC](../ntfs-specification.md)

# 6. Special streams

> Compression, alternate data streams, extended attributes, reparse
> points, the security catalogue, the upcase table, the volume file,
> and the `$Extend` family. Anything that lives in a non-`$DATA`
> attribute or in a metadata file under MFT records 3, 9, 10, or
> `$Extend` belongs here.

## Overview {#overview}

NTFS files and directories are containers for *attributes*, each
identified by a 4-byte type code. §2 covered the record envelope and
the resident/non-resident split; §3 covered run-list encoding for
non-resident `$DATA`; §4 covered `$INDEX_ROOT` and `$INDEX_ALLOCATION`;
§5 covered `$LogFile`. This section is the catch-all for the rest:
attributes that carry payloads other than plain file content, plus the
metadata files that hold cross-volume tables.

A "special stream" in this section means one of:

- A non-`$DATA` attribute whose payload has its own internal layout
  (`$REPARSE_POINT`, `$EA`, `$LOGGED_UTILITY_STREAM`).
- A named `$DATA` attribute used as a side channel rather than primary
  file content (Alternate Data Streams).
- A metadata file (record number ≤ 26) whose `$DATA` is consumed by the
  filesystem itself rather than by user processes (`$Volume`,
  `$UpCase`, `$Secure`, `$Extend\$Quota`, `$Extend\$ObjId`,
  `$Extend\$Reparse`).

LZNT1 compression is included because, although it lives inside
`$DATA`, the run-list interleaving it imposes is unique enough to
warrant its own treatment.

### Attribute type code map {#attr-type-map}

| Type code | Name                       | Notes                                                                |
| --------- | -------------------------- | -------------------------------------------------------------------- |
| `0x10`    | `$STANDARD_INFORMATION`    | Always resident. See [§2 std-info](02-mft-records.md#std-info).      |
| `0x20`    | `$ATTRIBUTE_LIST`          | Used when attributes span multiple MFT records.                      |
| `0x30`    | `$FILE_NAME`               | One per namespace per name. See [§2](02-mft-records.md#file-name).   |
| `0x40`    | `$OBJECT_ID`               | 16-byte GUID + optional birth IDs. See [§6.15](#objid).              |
| `0x50`    | `$SECURITY_DESCRIPTOR`     | Per-record SD; deprecated on user files since NTFS 3.0 (replaced by `$Secure`) but **mandatory on every system record (slots 0–11)** — see [§6 SDs on system records](#system-record-sds). |
| `0x60`    | `$VOLUME_NAME`             | Only on the `$Volume` system file. See [§6.13](#volume-file).        |
| `0x70`    | `$VOLUME_INFORMATION`      | NTFS major/minor version + flags. See [§6.13](#volume-file).         |
| `0x80`    | `$DATA`                    | Unnamed = primary content. Named = ADS. See [§6.2](#ads).            |
| `0x90`    | `$INDEX_ROOT`              | See [§4](04-indexes-directories.md).                                 |
| `0xA0`    | `$INDEX_ALLOCATION`        | See [§4](04-indexes-directories.md).                                 |
| `0xB0`    | `$BITMAP`                  | Allocation bitmap; per-attribute (e.g. `$I30:$BITMAP`).              |
| `0xC0`    | `$REPARSE_POINT`           | Symlinks, mount points, OneDrive stubs. See [§6.8](#reparse-attr).   |
| `0xD0`    | `$EA_INFORMATION`          | EA summary record. See [§6.4](#ea-info).                             |
| `0xE0`    | `$EA`                      | OS/2-style extended attributes. See [§6.5](#ea).                     |
| `0x100`   | `$LOGGED_UTILITY_STREAM`   | EFS / TxF metadata. See [§6.6](#logged-utility-stream).              |

`[UNVERIFIED]` for the
non-`$DATA` payload-bearing types listed above. Type `0x40`
(`$OBJECT_ID`), `0x50` (`$SECURITY_DESCRIPTOR`), `0x60`
(`$VOLUME_NAME`), and `0x70` (`$VOLUME_INFORMATION`) are
corroborated by `[OBSERVED:
src/record_build.rs]` (the writer emits each of these by code) and by
public Microsoft layout documentation referenced from
`[MS-FSCC §2.4]`.

### What this section does *not* cover

- MFT mechanics, record header, USA fixup → §2.
- Run-list encoding fundamentals → §3 (only the compressed-file run
  pattern is described here).
- `$I30` directory index keying / B+ tree → §4.
- `$LogFile` LSN structure or `USN_RECORD_V2` body → §5.

### Implementation status (`rust-fs-ntfs`)

| Stream / file              | Read | Write | Notes                                                                                  |
| -------------------------- | ---- | ----- | -------------------------------------------------------------------------------------- |
| Named `$DATA` (ADS)        | ✅   | ✅    | `fs_ntfs_write_named_stream` / `fs_ntfs_delete_named_stream` (`docs/STATUS.md`).       |
| LZNT1 compression          | ⛔   | ⛔    | No compression read or write path.                                                     |
| `$EA` / `$EA_INFORMATION`  | ✅   | ✅    | Resident only, MVP. See [`src/ea_io.rs`](../../../src/ea_io.rs).                       |
| `$REPARSE_POINT`           | ✅   | ✅    | Resident write of arbitrary tag + symlink helper.                                      |
| `$LOGGED_UTILITY_STREAM`   | 🟡   | ⛔    | Treated as opaque; preserved on read.                                                  |
| `$Secure` / `$SDS` / `$SII` / `$SDH` | ✅ | ✅ | mkfs writes canonical SD at security_id=0x100 with populated $SDH/$SII indexes and non-resident $SDS with 256 KiB mirror. Runtime SD insertion not implemented. |
| `$UpCase`                  | ✅   | ✅    | Canonical 128 KiB table. See [`src/upcase.rs`](../../../src/upcase.rs).                |
| `$Volume`                  | ✅   | ✅    | mkfs writes label + version. See [§6.13](#volume-file).                                |
| `$Extend` directory        | ✅   | ✅    | mkfs builds full $I30 directory at rec 11 with $ObjId (rec 16), $Reparse (rec 17), $Quota (rec 18) as VIEW_INDEX children. |

---

## $DATA (0x80) named streams — Alternate Data Streams {#ads}

A single MFT record can carry multiple `$DATA` attributes. The
*primary* stream is the one with an empty name (zero-length name
field). Any `$DATA` attribute with a non-empty name is an Alternate
Data Stream (ADS).

```
$DATA  (name = "")              ← primary content
$DATA  (name = "Zone.Identifier") ← Mark-of-the-Web
$DATA  (name = "favicon")        ← arbitrary user-attached metadata
```

The engine MUST validate each `$DATA` attribute
independently, and corruption in a named stream MUST NOT be allowed to
take down the unnamed stream or the entire MFT record. [UNVERIFIED]

### Naming {#ads-naming}

- Stream names are UTF-16LE, stored in the attribute name area between
  the resident/non-resident header and the value/run-list. `[MS-FSCC
  §2.4.4]` `[UNVERIFIED]`.
- Names are case-preserving; case-insensitive comparison uses
  `$UpCase` (see [§6.12](#upcase)) — same fold rule as filenames.
- An empty name distinguishes the unnamed primary stream. The C ABI
  in `rust-fs-ntfs` exposes named streams via
  `fs_ntfs_write_named_stream` / `fs_ntfs_delete_named_stream`
  (`[OBSERVED: docs/STATUS.md]`). The writer rejects an empty stream
  name to prevent collision with the primary `$DATA`.

### Coexistence with directories {#ads-directories}

Directories may legitimately carry named `$DATA` attributes
[UNVERIFIED]:

> Directories can legitimately contain named `$DATA` streams (e.g.
> `Zone.Identifier` or `com.apple.quarantine`). The index rebuilder
> MUST NOT assume that an `$INDEX_ROOT` attribute mutually excludes a
> `$DATA` attribute.

Both can coexist on the same record. Validation logic that walks
`$DATA` and `$INDEX_ROOT` MUST treat them as independent.

### chkdsk validation surface {#ads-chkdsk}

`chkdsk` validates each `$DATA` attribute's run-list independently.
A corrupt named-stream run-list produces a single attribute-record
diagnostic for the named attribute, not a record-level failure.
`[OBSERVED: docs/mkfs-bug-catalog.md]` confirms the
inverse: per-attribute corruption messages reference the attribute
type code and name (e.g. `Attribute record (30, "")`).

### Sizing fields on the `$FILE_NAME` mirror {#ads-sizing}

The `$FILE_NAME` attribute (`0x30`) carries `allocated_size` and
`real_size` fields that mirror the *primary* `$DATA` stream's
allocation and logical size. ADS sizes are **not** mirrored anywhere
in `$FILE_NAME`. `[OBSERVED: docs/mkfs-bug-catalog.md Bug 1]` —
mismatched mirror sizes for the primary stream produced
`Attribute record (30, "") is corrupt` errors on every system record
until the writer was fixed.

---

## LZNT1 compression {#lznt1}

NTFS natively compresses `$DATA` streams using the LZNT1 algorithm
(`[MS-XCA §2.5]` `[UNVERIFIED]`). Compression is
opt-in per attribute via the `is_compressed` flag in the attribute
header.

### Compression units {#lznt1-cu}

Data is divided into **Compression Units (CUs)**,
typically **16 clusters** = **64 KiB** with a 4 KiB cluster size. The
CU size is recorded as a power-of-two exponent in the non-resident
attribute header's `compression_unit_exponent` field
(`compression_unit_size = 1 << exponent` clusters).
`[UNVERIFIED]` against `[MS-NTFS]`.

A single bit flip inside a CU renders the entire CU unreadable —
LZNT1's dense back-reference encoding leaves no error-correcting
slack.

### Chunk header {#lznt1-chunk-header}

Each CU is decompressed as a sequence of independent **chunks**, each
covering at most **4096 bytes** (the LZ77 sliding-window size). A
chunk begins with a 2-byte little-endian header [UNVERIFIED]:

```
bit 15      bits 12–14    bits 0–11
┌─────────┐ ┌───────────┐ ┌──────────────┐
│ IsCompr │ │ Signature │ │ ChunkSize-1  │
│ (1 bit) │ │ (3 bits)  │ │ (12 bits)    │
└─────────┘ └───────────┘ └──────────────┘
```

| Field          | Meaning                                                                            |
| -------------- | ---------------------------------------------------------------------------------- |
| `IsCompressed` | bit 15: `1` = compressed payload follows; `0` = raw uncompressed.                  |
| `Signature`    | bits 12–14: MUST be `0b011` (= 3). Anything else indicates corruption.             |
| `ChunkSize-1`  | bits 0–11: chunk payload length (excluding the 2-byte header) minus 1. 0–4095.     |

A header value of `0x0000` terminates the stream [UNVERIFIED]. If
`IsCompressed = 0`, the next `ChunkSize+1` bytes are raw and copied
straight to the output.

### Compressed-chunk groups {#lznt1-groups}

A compressed chunk is a sequence of **tagged groups**. Each group
opens with a 1-byte **flag byte**; bits LSB-first describe the next 8
data elements [UNVERIFIED]:

- `0` = literal byte (1 byte; copy directly to output).
- `1` = back-reference tuple (2 bytes, little-endian).

The back-reference tuple encodes a `(displacement, length)` pair with
*dynamic* bit allocation that depends on how much output the chunk
has produced so far:

```
n             = max(ceil(log2(output_position + 1)), 4)
disp_bits     = n
length_bits   = 16 - n
displacement  = (tuple >> length_bits) + 1            // 1-based
length        = (tuple & ((1 << length_bits) - 1)) + 3 // min 3
```

The split point grows from 4 displacement / 12 length bits at the
start of the chunk to 12 displacement / 4 length bits near the end.
Decoders MUST recompute the split before decoding each
tuple. [UNVERIFIED]

### Termination, bounds {#lznt1-bounds}

[UNVERIFIED]:

- After each chunk, the decoder reads the next 2-byte header. `0x0000`
  terminates.
- Track the running uncompressed length and abort if it exceeds the
  attribute's `Data_Size` (which differs from `Allocated_Size` for
  compressed streams).
- A back-reference whose displacement points before the chunk's
  output start is corruption.

### Run-list shape for compressed `$DATA` {#lznt1-runs}

A compressed `$DATA` attribute's run-list is *not* a single contiguous
allocation. Each CU is independently compressed and its on-disk
allocation is sized to the compressed length. The shortfall between
the compressed allocation and the CU's logical size is filled with a
**sparse run** (run-list entry with `length = 0` LCN) that contributes
zero clusters but advances `VCN` by the missing amount. See §3 for the
sparse-run encoding (`[§3 sparse runs](03-data-runs-bitmap.md)`).

The result: for each CU you see one or more *real* runs followed by a
trailing sparse run that pads the CU back up to its full VCN extent.
The sum of real-cluster counts across the run-list equals the
attribute's `compressed_size` field; the sum of all run lengths
(including sparse) equals `data_size` rounded up to CU boundaries.
`[UNVERIFIED]` for the exact field-by-field layout — the
non-resident header field offsets for compressed attributes are not
enumerated.

### `valid_data_length` interaction {#lznt1-vdl}

`valid_data_length` (VDL) is the byte offset up to which the stream
contents are *defined*; bytes beyond VDL but below `data_size` read
as zero. For a compressed stream, VDL still applies in *uncompressed*
coordinates — the decompressor produces zero bytes for any output
position past VDL even if the underlying CUs decode to non-zero data.
`[UNVERIFIED]` — needs corroboration against `[MS-NTFS]`.

### Status in `rust-fs-ntfs`

Compression read and write are **not implemented**. The mkfs writer
emits no compressed streams; the read API does not transparently
decompress. The compression-unit exponent is left at `0` and the
`is_compressed` flag is never set. `[OBSERVED: docs/STATUS.md]`.

---

## $EA_INFORMATION (0xD0) {#ea-info}

Companion summary record for `$EA`. Resident, fixed-size payload.

The on-disk layout used by
the writer (`src/ea_io.rs::build_ea_information_value`) is:

```
Offset  Size  Field                  Description
------  ----  ---------------------  ---------------------------------------
0x00    2     EaPackedLength         Total bytes in the $EA value (FEA list)
0x02    2     EaQueryLength          Approximation of pack length (writer
                                     emits same value as EaPackedLength)
0x04    4     EaCount (NEED_EA)      Count of FEAs with FILE_NEED_EA flag
                                     set in their flags byte
```

`[OBSERVED: src/ea_io.rs]`. The writer treats `EaQueryLength` as an
approximation of `EaPackedLength` rather than a true upper bound on
the response buffer required by `NtQueryEaFile`; the
cross-validation rule only requires `EaPackedLength`
and the NEED_EA count to be self-consistent with the `$EA` body —
not a particular `EaQueryLength` value.

### Cross-validation rules {#ea-info-validation}

[UNVERIFIED]:

- `EaPackedLength` MUST equal the sum of every FEA entry's encoded
  size (header + name + null terminator + value, padded to the next
  4-byte boundary).
- The count of `$EA` entries with `FILE_NEED_EA` set MUST equal the
  `EaCount (NEED_EA)` field.
- A mismatch is flagged as stale-summary corruption. The repair
  policy is destructive (delete *both* `$EA` and `$EA_INFORMATION`)
  — the file's primary `$DATA` is preserved.

---

## $EA (0xE0) {#ea}

`$EA` carries a packed list of `FILE_FULL_EA_INFORMATION` entries
`[MS-FSCC §2.4.15]` `[UNVERIFIED]` (cited inline in
`src/ea_io.rs`'s module doc). The on-disk encoding the writer emits
is:

```
Offset  Size  Field             Description
------  ----  ----------------  -----------------------------------
0x00    4     NextEntryOffset   Bytes to next entry, or 0 = last
0x04    1     Flags             0x80 = FILE_NEED_EA
0x05    1     EaNameLength      Bytes; max 254
0x06    2     EaValueLength     Bytes; max 65535
0x08    var   EaName            ASCII; bytes 0x20–0x7E only
+name   1     NUL terminator    0x00 (always present)
+1      var   EaValue           Arbitrary bytes
+value  var   pad               Pad to next 4-byte boundary
```

`[OBSERVED: src/ea_io.rs]`.

### Encoded-size rule {#ea-size}

```
entry_encoded_size(name_len, value_len) =
    align4(8 + name_len + 1 + value_len)
```

`[OBSERVED: src/ea_io.rs::entry_encoded_size]`.

### Validation chain (5 rules) {#ea-validation}

[UNVERIFIED]:

1. **Chain integrity.** Walk `NextEntryOffset`. Each offset MUST be
   DWORD-aligned (divisible by 4). The cursor MUST NOT exceed the
   declared `$EA` value length. Zero terminates.
2. **Name bounds.** `EaNameLength + EaValueLength + fixed_header` MUST
   be `≤` the current entry's declared size.
3. **ASCII name constraint.** `EaName` is ASCII (bytes 0x20–0x7E).
   Any non-ASCII byte is structural corruption. `$EA` names are
   *not* Unicode — even when used to back POSIX xattrs from Linux
   drivers, the names remain ASCII (e.g. `user.comment`,
   `security.selinux`).
4. **Null terminator.** `EaName[EaNameLength] == 0x00`. Missing NUL
   is corruption.
5. **Cross-validation with `$EA_INFORMATION`.** See [§6.4
   cross-validation](#ea-info-validation) above.

### Size limits {#ea-size-limits}

- `EaNameLength` is a `u8`; the writer rejects names longer than 254
  bytes (preserving the implicit NUL byte slot)
  `[OBSERVED: src/ea_io.rs::encode]`.
- `EaValueLength` is a `u16`, so values cap at 65 535 bytes per
  entry `[OBSERVED: src/ea_io.rs::encode]`.
- Total `$EA` size: bounded by Windows' per-file EA limit (typically
  64 KiB across all FEAs). `[UNVERIFIED]` against `[MS-FSCC]`.

### NEED_EA semantics {#ea-need-ea}

`Flags` bit `0x80` is `FILE_NEED_EA` (Windows OS/2 compatibility
heritage). When set, the EA is "critical": Windows refuses to open
the file from a process that does not understand its EAs.
`[OBSERVED: src/ea_io.rs::FLAG_NEED_EA]`.

The writer's `count_need_ea` walker is what produces the
`EaCount (NEED_EA)` field for `$EA_INFORMATION`.

### Corruption policy {#ea-corruption}

A partial EA list is forbidden — truncating the chain
to "save" some entries leaves `EaCount` inconsistent and Windows
crashes on access. Both `$EA` and `$EA_INFORMATION` are deleted
together; primary `$DATA` is preserved. [UNVERIFIED]

### Status in `rust-fs-ntfs`

Resident `$EA` is fully supported (read + write + delete). Non-resident
`$EA` is rejected at read time with
`"$EA is non-resident (MVP only supports resident EAs)"`
`[OBSERVED: src/ea_io.rs::read_from_record]`.

---

## $LOGGED_UTILITY_STREAM (0x100) {#logged-utility-stream}

An opaque, internally-logged byte stream used by Windows
sub-systems [UNVERIFIED]. The two known instances are:

- `$EFS` — Encrypting File System metadata (key wrappers, certificate
  blobs).
- `$TXF_DATA` — Transactional NTFS metadata. Deprecated since
  Windows 8.

The internal layout is **not publicly documented** in detail, except
for the `$EFS` header (see [§6.7](#efs-header)).

### Treatment policy {#lus-policy}

[UNVERIFIED]:

- Treat structurally as a *named `$DATA`*. Non-resident: parse runs
  the same way (§3); resident: treat the value as opaque bytes.
- NEVER parse or interpret the payload content. The format is
  version-dependent and undocumented.
- On run-list corruption, delete only the offending
  `$LOGGED_UTILITY_STREAM` attribute. The primary `$DATA` is
  preserved.

### Zombie protection {#lus-zombie}

A file with only `$LOGGED_UTILITY_STREAM` and no `$DATA` is **not** a
zombie. Encrypted files legitimately have their content stored in
`$DATA` *with the cleartext re-encrypted in place* (so `$DATA` exists
but is opaque ciphertext) and their key material in `$EFS`. Orphan
recovery passes MUST NOT prune records that have an
`$LOGGED_UTILITY_STREAM`. [UNVERIFIED]

### `$TXF_DATA` policy {#lus-txf}

[UNVERIFIED]:

- No explicit handling for type-`0x100` payloads with the `$TXF_DATA`
  name.
- If non-resident, verify the LSNs inside the payload point to valid
  `$LogFile` bounds without attempting replay. Delete on out-of-bounds
  LSNs (an orphaned transaction can lock files permanently).
- Otherwise, treat as a generic `$LOGGED_UTILITY_STREAM`.

### Status in `rust-fs-ntfs`

Read path preserves `$LOGGED_UTILITY_STREAM` attributes opaquely.
Write path does not synthesize them. `[OBSERVED: docs/STATUS.md]`.

---

## EFS_ATTR_HEADER {#efs-header}

Header of the `$EFS` `$LOGGED_UTILITY_STREAM` payload.
24-byte fixed prefix, followed by DDF / DRF arrays at offsets named in
the prefix. [UNVERIFIED]

```
Offset  Size  Field              Description
------  ----  -----------------  -----------------------------------------
0x00    4     Length             Total EFS attribute data size (must
                                 equal the attribute's value length).
0x04    4     State              0x00000000 = decrypted (metadata only);
                                 other values are recognized states.
0x08    4     Version            0x00000002 = EFS v2; 0x00000003 = EFS v3.
0x0C    4     CryptoApiVersion   Crypto API version used to wrap the FEK.
0x10    4     DDF_Offset         Offset (from 0x00) to Data Decryption
                                 Field. MUST be ≥ 0x1C and < Length.
0x14    4     DRF_Offset         Offset to Data Recovery Field. 0 if no
                                 DRA, else ≥ 0x1C and < Length.
0x18    4     Reserved           MUST be 0.
```

`[UNVERIFIED]`.

### DDF / DRF body {#efs-ddf-drf}

At `DDF_Offset` (and `DRF_Offset` if present) [UNVERIFIED]:

```
+0x00   4     Count               Number of EFS_CERTIFICATE_BLOB entries
+0x04   var   EFS_CERTIFICATE_BLOB[]
                                  Each: SID hash, certificate thumbprint,
                                  encrypted FEK (File Encryption Key).
```

The `EFS_CERTIFICATE_BLOB` entry layout itself is not enumerated.
`[UNVERIFIED]`.

### Lightweight header validation {#efs-validation}

Optional structural-only checks [UNVERIFIED]:

- `Length` MUST equal the attribute's data length.
- `State` MUST be `0x00000000` or a recognized state.
- `Version` MUST be `2` or `3`.
- `DDF_Offset` MUST be `≥ 0x1C` and `< Length`.
- `DRF_Offset` MUST be `0` *or* `≥ 0x1C` and `< Length`.

Failing any of these → delete the `$LOGGED_UTILITY_STREAM` attribute.
The crypto payload is not decrypted at any point — header validation
checks structure only.

---

## $REPARSE_POINT (0xC0) {#reparse-attr}

`$REPARSE_POINT` redirects file or directory accesses
into another path or into a filter driver (cloud sync, dedup, HSM).
A corrupted reparse point can make an entire directory tree
inaccessible. [UNVERIFIED]

### REPARSE_DATA_BUFFER header {#reparse-header}

```
Offset  Size  Field              Description
------  ----  -----------------  -----------------------------------------
0x00    4     ReparseTag         Identifies the reparse type. See tag
                                 table below.
0x04    2     ReparseDataLength  Length of the tag-specific payload that
                                 follows. MUST NOT exceed 16384 (16 KiB).
0x06    2     Reserved           MUST be 0x0000.
0x08    16    GUID               Present only for non-Microsoft tags
                                 (bit 31 of ReparseTag clear). Microsoft
                                 tags omit this field.
0x08 / 0x18  var  Payload         Tag-specific. Header size = 8 for
                                 Microsoft tags, 24 for third-party.
```

`[OBSERVED: src/record_build.rs::build_resident_reparse_point_attribute]`. The
GUID-presence rule is `[UNVERIFIED]` against `[MS-FSCC §2.1.2]` —
`rust-fs-ntfs` writes only the 8-byte form.

### Tag bit layout {#reparse-tag-bits}

`ReparseTag` is a 32-bit value [UNVERIFIED]:

| Bit     | Meaning                                                               |
| ------- | --------------------------------------------------------------------- |
| 31 (M)  | `1` = Microsoft-assigned tag; `0` = third-party.                      |
| 30 (R)  | Reserved. Exact semantics not specified in any source we have. `[UNVERIFIED]` |
| 29 (N)  | Name-surrogate flag. `[UNVERIFIED]` against `[MS-FSCC §2.1.2.1]`.     |
| 28 (D)  | Directory bit. `[UNVERIFIED]` against `[MS-FSCC §2.1.2.1]`.           |
| 27..16  | Reserved.                                                             |
| 15..0   | Tag value.                                                            |

The M/N/D bit positions beyond bit 31 (Microsoft vs third-party) are
not enumerated in any source we have. The split above mirrors the
public Microsoft documentation convention; bit-for-bit confirmation
is `[UNVERIFIED]`.

### Validation rules {#reparse-validation}

[UNVERIFIED]:

1. **Tag validity.** Recognised, well-formed values only. All-zero or
   all-ones (`0xFFFFFFFF`) tag values are structurally impossible.
2. **Size bounds.** `ReparseDataLength ≤ 16384`.
3. **Flag consistency.** The MFT record's
   `$STANDARD_INFORMATION.FileAttributes` MUST have
   `FILE_ATTRIBUTE_REPARSE_POINT` (`0x0400`) set if a reparse
   attribute is present, and clear otherwise.
4. **`$Reparse` index cross-check.** A corresponding entry MUST exist
   in `$Extend\$Reparse` (`$R` index). See [§6.17](#reparse-index).

### Corruption actions {#reparse-corruption}

[UNVERIFIED]:

- *Invalid tag or size overflow:* delete the `$REPARSE_POINT`
  attribute and clear `FILE_ATTRIBUTE_REPARSE_POINT` from
  `$STANDARD_INFORMATION`. The file becomes a regular file, which is
  always safe.
- *Flag mismatch only:* surgical fix — toggle the SI flag to match
  the attribute's presence/absence.
- *Missing `$Reparse` index entry:* re-insert during the `$Extend`
  index pass.

### Circular-reference detection {#reparse-cycles}

When resolving a reparse point during `$Extend` index
verification, if the target MFT reference resolves back to the
original file, it's a fatal cycle. The repair engine MUST implement
cycle detection (e.g. Floyd's tortoise-and-hare). On cycle detection,
delete the offending reparse attribute and emit `E_REPARSE_CIRCULAR`.
[UNVERIFIED]

### Status in `rust-fs-ntfs`

The writer emits resident `$REPARSE_POINT` attributes via
`fs_ntfs_write_reparse_point` (arbitrary tag + data) and a
`fs_ntfs_create_symlink` convenience that builds a
`SymbolicLinkReparseBuffer` payload `[OBSERVED:
src/record_build.rs::build_resident_reparse_point_attribute,
build_symlink_reparse_data]`. `FILE_ATTRIBUTE_REPARSE_POINT` is set
on write and cleared on `fs_ntfs_remove_reparse_point` `[OBSERVED:
docs/STATUS.md]`. The `$Extend\$Reparse` index entry is **not**
maintained by the writer — see [§6.17](#reparse-index).

---

## Reparse tag table {#reparse-tags}

The tags below are the minimum the repair engine must accept. The
same constants appear in
[`src/record_build.rs::reparse_tag`](../../../src/record_build.rs)
where the writer emits them. [UNVERIFIED]

| Tag                                | Value         | Source                                       |
| ---------------------------------- | ------------- | -------------------------------------------- |
| `IO_REPARSE_TAG_MOUNT_POINT`       | `0xA0000003`  | `[UNVERIFIED]`                               |
| `IO_REPARSE_TAG_SYMLINK`           | `0xA000000C`  | `[UNVERIFIED]`                               |
| `IO_REPARSE_TAG_DEDUP`             | `0x80000013`  | `[UNVERIFIED]`                               |
| `IO_REPARSE_TAG_NFS`               | `0x80000014`  | `[UNVERIFIED]`                               |
| `IO_REPARSE_TAG_WCI`               | `0x80000018`  | `[UNVERIFIED]`                               |
| `IO_REPARSE_TAG_CLOUD`             | `0x9000001A`  | `[UNVERIFIED]`                               |
| `IO_REPARSE_TAG_WOF`               | `0x80000017`  | `[OBSERVED: src/record_build.rs]` `[UNVERIFIED]` |
| `IO_REPARSE_TAG_APPEXECLINK`       | `0x8000001B`  | `[OBSERVED: src/record_build.rs]` `[UNVERIFIED]` |
| `IO_REPARSE_TAG_LX_SYMLINK`        | `0xA000001D`  | `[OBSERVED: src/record_build.rs]` `[UNVERIFIED]` |

### Mount-point payload {#reparse-payload-mount}

`MOUNT_POINT_REPARSE_DATA_BUFFER` [UNVERIFIED]:

```
Offset  Size  Field                  Description
------  ----  ---------------------  ------------------------------------
0x00    2     SubstituteNameOffset   Offset within PathBuffer
0x02    2     SubstituteNameLength   Bytes of UTF-16 substitute name
0x04    2     PrintNameOffset        Offset within PathBuffer
0x06    2     PrintNameLength        Bytes of UTF-16 print name
0x08    var   PathBuffer             SubstituteName then PrintName
                                     (UTF-16LE, NOT null-terminated)
```

Validation [UNVERIFIED]:

- `SubstituteNameOffset + SubstituteNameLength ≤ ReparseDataLength - 8`.
- Same for `PrintName`.
- Both offsets MUST be WCHAR-aligned (divisible by 2).

The substitute name is typically an NT-style path
(`\??\Volume{GUID}\` or `\??\C:\Target`). The print name is what
Explorer displays.

### Symlink payload {#reparse-payload-symlink}

`SYMBOLIC_LINK_REPARSE_DATA_BUFFER`. Same as the
mount-point payload, plus a 4-byte `Flags` field after the four
length/offset fields [UNVERIFIED]:

```
Offset  Size  Field        Description
------  ----  -----------  --------------------------------------------
0x08    4     Flags        0x00000000 = absolute path
                           0x00000001 = relative path
0x0C    var   PathBuffer   SubstituteName then PrintName
```

`[OBSERVED: src/record_build.rs::build_symlink_reparse_data]`. A
zero-length `PrintName` is valid (common for relative symlinks).

### Dedup stub policy {#reparse-payload-dedup}

A deduplicated file is a stub whose primary unnamed
`$DATA` has `Allocated_Size = 0` (no clusters allocated) but
`Data_Size = original_logical_size`. The reparse point redirects
reads into a chunk store. The data-run parser MUST NOT flag
`Allocated_Size = 0` with `Data_Size != 0` as corruption when a
`IO_REPARSE_TAG_DEDUP` reparse point is present. [UNVERIFIED]

---

## $Secure (record 9) {#secure-file}

`$Secure` is a metadata file at MFT record 9 that holds the volume's
security descriptor catalogue. Per-record `$SECURITY_DESCRIPTOR`
(type `0x50`) was the NTFS 1.2 mechanism; NTFS 3.0+ moved security
descriptors into the catalogued `$Secure` file and replaced per-record
SDs with a 32-bit `security_id` integer field in
`$STANDARD_INFORMATION` (cross-link [§2 std-info](02-mft-records.md#std-info)).
`[UNVERIFIED]` against `[MS-NTFS]` for the version-cutover claim.

### Streams of `$Secure` {#secure-streams}

- `$SDS` — unnamed-named: the named stream `:$DATA:$SDS` carries the
  packed catalogue of security descriptors, one entry per
  `security_id`.
- `$SII` — `$INDEX_ROOT` / `$INDEX_ALLOCATION` named `:$SII`. Index
  keyed by `security_id` (4-byte integer); used to look up the SDS
  offset for a given ID.
- `$SDH` — `$INDEX_ROOT` / `$INDEX_ALLOCATION` named `:$SDH`. Index
  keyed by the security descriptor *hash + security_id*; used to
  detect descriptor reuse during inserts.

Cross-link to [§4 indexes](04-indexes-directories.md) for the B+ tree
mechanics that `$SII` and `$SDH` share with `$I30`.

`[UNVERIFIED]`.

### `security_id` reference from `$STANDARD_INFORMATION` {#secure-sid-ref}

Each MFT record's `$STANDARD_INFORMATION` carries a `security_id`
field (32-bit, NTFS 3.0+) that names a row in `$SDS` via the `$SII`
index. Multiple files share a single `security_id` when their SDs are
identical — the catalogue deduplicates. Cross-link to
[§2 std-info](02-mft-records.md#std-info) for the field offset.

### Per-record `$SECURITY_DESCRIPTOR` on system records (slots 0–11) {#system-record-sds}

Although NTFS 3.0+ moved per-record SDs into the `$Secure` catalogue
for user files, **every system MFT record (slots 0..=11) still
carries a per-record `$SECURITY_DESCRIPTOR` attribute (type `0x50`)
on a modern `format.com` volume**
[`[OBSERVED: docs/chkdsk-improvement-findings.md §2.5.1]`](#references).
Omitting it on system records produces the chkdsk `frs.cxx 60f`
internal assertion plus Event 55 at mount.

Reference `format.com` ships one of three byte-verbatim SD blobs
per system record, distinguished by DACL access mask:

| Blob              | Used by                                                                              | Size      | DACL access mask                                                                                       |
| ----------------- | ------------------------------------------------------------------------------------ | --------- | ------------------------------------------------------------------------------------------------------ |
| `SD_SYSFILE_RO`   | `$MFT(0)`, `$MFTMirr(1)`, `$LogFile(2)`, `$AttrDef(4)`, `$Bitmap(6)`, `$Boot(7)`, `$BadClus(8)`, `$UpCase(10)` | 104 bytes | `0x00120089` — `SYNCHRONIZE \| READ_CONTROL \| FILE_READ_DATA \| FILE_READ_EA \| FILE_READ_ATTRIBUTES` |
| `SD_SYSFILE_RW`   | `$Volume(3)`, `$Quota`/`$Secure(9)`, `$Extend(11)`                                    | 104 bytes | `0x0001009F` — RO bits plus `DELETE \| FILE_WRITE_DATA \| FILE_APPEND_DATA \| FILE_WRITE_EA`             |
| `SD_ROOT_DIR`     | root `.`(5)                                                                          | 248 bytes | Wider DACL with `INHERIT_ONLY` ACEs that propagate to children                                          |

All three are standard `SECURITY_DESCRIPTOR_RELATIVE`
([MS-DTYP §2.4.6](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-dtyp/)):
`Revision=1`, `Control=0x8004` (`SE_DACL_PRESENT | SE_SELF_RELATIVE`),
Owner = `BUILTIN\Administrators` (`S-1-5-32-544`), Group = same, no
SACL, self-relative DACL.

Canonical 104-byte `SD_SYSFILE_RO` (the variant 8 of 12 system
records use):

```
header (20 bytes):
  01 00          rev=1, Sbz1=0
  04 80          Control = SE_SELF_RELATIVE | SE_DACL_PRESENT
  48 00 00 00    OffsetOwner = 72
  58 00 00 00    OffsetGroup = 88
  00 00 00 00    OffsetSacl  = 0   (no SACL)
  14 00 00 00    OffsetDacl  = 20

DACL @20 (52 bytes):
  rev=2, Sbz1=0, AclSize=0x34, AceCount=2, Sbz2=0
  ACE[0] @28 (20B): ACCESS_ALLOWED, mask=0x00120089, SID=S-1-5-18 (NT AUTHORITY\SYSTEM)
  ACE[1] @48 (24B): ACCESS_ALLOWED, mask=0x00120089, SID=S-1-5-32-544 (BUILTIN\Administrators)

Owner SID @72 (16 bytes): S-1-5-32-544
Group SID @88 (16 bytes): S-1-5-32-544
```

The `RW` variant differs at exactly four bytes (offsets 32, 33, 52,
53) where the access mask becomes `0x0001009F`.

These three blobs are baked into `rust-fs-ntfs::mkfs` as the
constants `SD_SYSFILE_RO`, `SD_SYSFILE_RW`, `SD_ROOT_DIR` selected by
`sd_for_system_record(rec_num)`
[`[OBSERVED: src/mkfs.rs:82-132, 170-176]`](#references).

Note that **rec 9 (`$Quota` / `$Secure`) uses the RW variant** — even
though it is the security catalogue itself, its own per-record SD is
still required and is writeable (chkdsk would otherwise reject a
fresh-format volume).

### Validation pass {#secure-validation}

[UNVERIFIED]:

1. Walk `$SDS` linearly, parsing each entry's header.
2. Boundary check: an entry MUST NOT cross a 256 KiB boundary.
3. Self-referential offset check: each entry's `offset` field MUST
   match the entry's actual byte offset within `$SDS`. If not, fall
   back to the redundant copy 256 KiB later.
4. Hash verification: recompute `compute_sii_hash` over the
   descriptor body (`offset+0x14 .. offset+entry.length`) and compare
   against the entry's `hash_key` field.
5. Cross-check `$SII`: every `security_id` referenced must appear in
   the validated `$SDS` set; orphaned `$SII` entries are deleted.
6. Cross-check `$SDH`: every hash in `$SDH` must resolve to a valid
   `$SDS` entry; orphans deleted.

The `compute_sii_hash` algorithm `[OBSERVED: src/sds.rs::sdh_hash —
verified against 12 entries from a Microsoft Format-Volume NTFS v3.1
reference volume; all 12 matched]`:

```
hash = 0
for each 4-byte LE dword in the SD body:
    hash = ((hash >> 29) | (hash << 3)) + dword   // ror 29 + add
    hash = hash & 0xFFFFFFFF                       // 32-bit truncate
```

i.e. a rotate-right-29 (= rotate-left-3) accumulator with 32-bit
truncation. Used both as the `$SDH` key prefix and as the
`SDS_Entry.hash_key` field. `[OBSERVED: src/sds.rs]`

### Status in `rust-fs-ntfs`

`$Secure` is fully implemented at format time `[OBSERVED: src/mkfs.rs:938-1088, src/sds.rs]`:

- Non-resident `$SDS` data stream with one canonical SD entry at `security_id = 0x100`.
- Populated `$SDH` index (keyed by `sdh_hash(SD_body) + security_id`).
- Populated `$SII` index (keyed by `security_id`).
- All system MFT records (slots 0–18) carry `$STANDARD_INFORMATION.security_id = 0x100`.
- 256 KiB mirror: each SDS entry is duplicated at `primary_offset + 0x40000`.

Runtime insertion of new SD entries (for per-file ACLs) is not implemented — all
fresh-format files share the single default SD. `fs_ntfs_set_security_id` can point a
file at the existing `0x100` entry.

---

## $SDS layout {#sds-layout}

The `:$SDS` stream is a sequence of
fixed-header + variable-payload entries. Each entry header
`[OBSERVED: src/sds.rs — field offsets SDS_HDR_HASH_OFF=0,
SDS_HDR_SECURITY_ID_OFF=4, SDS_HDR_ENTRY_OFFSET_OFF=8,
SDS_HDR_ENTRY_SIZE_OFF=16, SDS_HDR_SD_DATA_OFF=20]`:

```
Offset  Size  Field          Description
------  ----  -------------  -----------------------------------------
0x00    4     hash_key       SII hash of the descriptor body
0x04    4     security_id    Catalogue ID (matches $SII / SI field)
0x08    8     offset         Self-referential: this entry's byte
                             offset within the $SDS stream
0x10    4     length         Total entry length (header + SD + pad)
0x14    var   security_descriptor
                             SECURITY_DESCRIPTOR_RELATIVE; size =
                             length - 0x14 (less trailing pad)
+SD     var   pad            Pad to 16-byte boundary
```

`[OBSERVED: validate_secure
boundary / self-offset / hash checks]`.

### 256 KiB mirror {#sds-mirror}

Every `$SDS` entry is mirrored at offset
`+0x40000` (256 KiB) for redundancy. Validation prefers the primary
copy; on self-offset mismatch the validator reads the mirror. Entries
MUST NOT span a 256 KiB boundary — the mirror granularity assumes
each block is self-contained.
`[OBSERVED: src/sds.rs SDS_MIRROR_GAP = 0x40000; build_sds mirrors
each primary entry verbatim at primary_offset + 0x40000]`

`align_16(...)` is used to advance the cursor: each entry occupies
`align16(length)` bytes, ensuring the next entry's header is 16-byte
aligned.

### `SECURITY_DESCRIPTOR_RELATIVE` body {#sds-sd-body}

The per-entry SD is a `SECURITY_DESCRIPTOR_RELATIVE`
(`[MS-DTYP §2.4.6]` `[UNVERIFIED]`). Its internal structure
(revision, control flags, owner SID, group SID, SACL, DACL) is treated
as opaque bytes for hash computation purposes. Cross-reference
`[MS-DTYP]` for the SD body layout if implementing an active reader;
treat as opaque for write-side correctness.

---

## $UpCase (record 10) {#upcase}

`$UpCase` lives at MFT record 10 and contains a flat 128 KiB array of
65 536 little-endian `u16` values: `upcase[c]` is the uppercase
folding of BMP code point `c`. `[CORROBORATED: OBSERVED:
src/upcase.rs, OBSERVED: src/upcase-canonical.bin]`.

```
Layout: 65536 × u16 LE = 131072 bytes (128 KiB)
upcase[c] = uppercase form of BMP code unit c
```

### Use site {#upcase-collation}

`$UpCase` is consumed by every `COLLATION_FILE_NAME` comparison —
`$I30`, `$O` (object ID), `$Q` (quota), and any other index whose key
is a UTF-16 filename. See [§4 indexes](04-indexes-directories.md) for
where collation hooks in. The fold rule:

1. For each pair of UTF-16 code units `(a, b)` from the two names,
   substitute `a' = upcase[a]` and `b' = upcase[b]`.
2. Compare folded code units numerically.
3. On full prefix match, the shorter name sorts first.

`[OBSERVED: src/upcase.rs::cmp_names]`. Surrogates (the upper /
lower-surrogate range) are left unfolded — NTFS only collates within
BMP per `COLLATION_FILE_NAME`.

### Canonical-vs-Unicode divergence {#upcase-canonical}

The on-disk table MUST match Microsoft's canonical NT 3.x mapping
**byte-for-byte**. An earlier `rust-fs-ntfs` revision synthesised the
table from `char::to_uppercase()` (modern Unicode rules). It diverged
from Microsoft's table at **327 BMP code points**. The canonical
example: `U+00B5 MICRO SIGN` — modern Unicode uppercases to
`U+039C GREEK CAPITAL LETTER MU`; NTFS preserves it as `U+00B5`.
`[OBSERVED: src/upcase.rs]`.

When the on-disk table differs from chkdsk's built-in copy, chkdsk
reports:

```
Read-only chkdsk found bad on-disk uppercase table — using system table
```

and falls back to its built-in table for the remainder of the run.
This causes filename collation in chkdsk's view to disagree with
collation in the on-disk indexes, producing spurious "out-of-order"
or "duplicate" entries. `[OBSERVED: docs/chkdsk-improvement-findings.md §2.6 upcase
note]`.

### Canonical bytes embedded in the writer {#upcase-blob}

`rust-fs-ntfs` ships the canonical 128 KiB at
[`src/upcase-canonical.bin`](../../../src/upcase-canonical.bin)
(SHA256 `41c26bc7a12bdaeb26025c93118697c7e3ef81ee048b00fe5cce2a472e0e0742`).
The writer's `generate_upcase_table()` returns a copy of this blob
verbatim. The blob was captured byte-for-byte from a Microsoft
`format.com /FS:NTFS` reference volume via raw read of MFT record
10's `$DATA` — no GPL/LGPL or third-party Linux NTFS driver code was
consulted. `[OBSERVED: src/upcase.rs module doc]`.

### Reader path {#upcase-reader}

`UpcaseTable::load` uses upstream's non-resident `$DATA` walker to
fetch the 128 KiB without re-implementing run-list traversal. The
reader rejects volumes whose `$UpCase` value length is `< 128 KiB`.
`[OBSERVED: src/upcase.rs::load_from_reader]`.

---

## $Volume (record 3) {#volume-file}

The `$Volume` system file at MFT record 3 carries volume-wide
metadata via two attributes that exist *only* on this record:

### `$VOLUME_NAME` (0x60) {#volume-name}

UTF-16LE volume label, NOT null-terminated. Length is the attribute's
resident value length. May be empty (zero-length).

`[OBSERVED: rec 3 $VOLUME_NAME byte-decode (raw MFT read corroborates UTF-16 LE; chkdsk console as ?????? is codepage rendering, not bytes-on-disk error)
byte-perfect"]` confirmed via raw MFT read that
`$VOLUME_NAME` carries pure UTF-16LE — chkdsk's stdout rendering as
`??????` for CJK labels is a console codepage limitation, not a
bytes-on-disk error.

### `$VOLUME_INFORMATION` (0x70) {#volume-information}

Fixed 12-byte payload `[UNVERIFIED]` against `[MS-NTFS]`:

```
Offset  Size  Field            Description
------  ----  ---------------  -----------------------------------------
0x00    8     Reserved         Should be 0
0x08    1     MajorVersion     NTFS major version (3 for NTFS 3.x)
0x09    1     MinorVersion     NTFS minor version (1 for NTFS 3.1)
0x0A    2     Flags            VOLUME_DIRTY (0x0001), VOLUME_RESIZE_LOG_FILE
                               (0x0002), VOLUME_UPGRADE_ON_MOUNT (0x0004),
                               VOLUME_MOUNTED_ON_NT4 (0x0008),
                               VOLUME_DELETE_USN_UNDERWAY (0x0010),
                               VOLUME_REPAIR_OBJECT_ID (0x0020),
                               VOLUME_MODIFIED_BY_CHKDSK (0x0080),
                               others reserved.
```

The flag-bit values above follow Tuxera's public documentation and
have been cross-checked against `format.com`'s output via byte-diff
[`[OBSERVED: src/mkfs.rs:836-880]`](#references). Note specifically
that `VOLUME_MODIFIED_BY_CHKDSK = 0x0080` — older public references
sometimes quote `0x8000`, which is wrong (it doesn't match what
Microsoft's `format.com` actually writes).

#### Fresh-format shape {#volume-information-fresh-format}

`format.com` and `rust-fs-ntfs`'s `mkfs` both stamp the same shape
on a freshly-formatted volume
[`[OBSERVED: src/mkfs.rs:836-881]`](#references):

| Field         | Value                                          |
| ------------- | ---------------------------------------------- |
| MajorVersion  | `1`                                            |
| MinorVersion  | `2`                                            |
| Flags         | `0x0080` (`VOLUME_MODIFIED_BY_CHKDSK` alone)   |

The `1.2 + UPGRADE_ON_MOUNT` configuration is a Windows convention
that the format step has not yet been "blessed" by chkdsk's full
catalog of structural transitions; `ntfs.sys` upgrades the volume
to `3.1` with the `UPGRADE_ON_MOUNT` flag cleared on the first RW
mount.

`MODIFIED_BY_CHKDSK = 0x0080` is set at format time because Windows
runs an implicit chkdsk pass during `format.com`'s finalize step.
Without this flag set, `ntfs.sys`'s mount path queues a proactive
scan on every mount and `chkdsk /scan` exits 13 even on a structurally
sound volume.

`rust-fs-ntfs` rewrites the version-pair to `3.1` with all upgrade
flags cleared at the next opportunity, on the RW-mount path
(`fsck::upgrade_volume_version`, see [§5 fsck](../README.md) and
[`docs/STATUS.md`](../STATUS.md)). The transition is idempotent and
mimics what `ntfs.sys` would do.

#### Why earlier code emitted `0x0084` — and why that was wrong

An earlier `rust-fs-ntfs` `mkfs` revision shipped `Flags = 0x0084`
(adding `VOLUME_UPGRADE_ON_MOUNT = 0x0004`). That value was cribbed
from a post-chkdsk-rollback test fixture that already had Stage 1
corruption, not from a clean `format.com` output. The
`UPGRADE_ON_MOUNT` bit on a `chkdsk /scan` snapshot mount drives
`ntfs.sys` to a Critical Event 55 because the read-only snapshot
can't complete the upgrade. Dropping that bit (so the volume ships
as `1.2 + 0x0080` instead of `1.2 + 0x0084`) eliminated the Critical
Event 55 and is the byte sequence currently stamped on the matrix's
sealed runs `[OBSERVED: test-diagnostics/matrix-results.json]`.

### NTFS version cutoffs {#volume-ntfs-versions}

Cross-link to [§1 geometry](sections/01-geometry-boot.md) (which owns
the per-version feature matrix). Recap:

- `1.2` = NT 3.51 / 4.0. Per-record `$SECURITY_DESCRIPTOR`. No
  `$Secure` catalogue.
- `3.0` = Win 2000. `$Secure` introduced; per-record SD attributes
  retired (replaced by `security_id` reference field).
- `3.1` = XP+. Current default.

`[UNVERIFIED]` against `[MS-NTFS]` for the exact mapping — this is
the conventional historical attribution.

### `$Volume` GUID

The boot-sector volume serial number (8 bytes) is **not** the same as
a "volume GUID". `$Volume` itself does not carry a GUID attribute.
The `$Extend\$ObjId` mechanism (see [§6.15](#objid)) is the closest
thing to a per-file GUID, not a per-volume one. `[UNVERIFIED]`.

---

## $Extend directory (record 11) {#extend-directory}

`$Extend` is a directory at MFT record 11 that holds NTFS 3.0+
metadata children. It looks like a regular directory: it has
`$INDEX_ROOT:$I30` (and `$INDEX_ALLOCATION:$I30` if needed) and is
keyed by COLLATION_FILE_NAME like any other directory.

### Children {#extend-children}

| Path                | Record # | Stream(s)                                           | Section                  | Notes |
| ------------------- | -------- | --------------------------------------------------- | ------------------------ | ----- |
| `$Extend\$ObjId`    | 16       | `:$O` (`$INDEX_ROOT` + `$INDEX_ALLOCATION`)         | [§6.15](#objid)          | `[OBSERVED: src/mkfs.rs — record 16/17/18 respectively; conventional NTFS uses 24/25/26 but Windows accepts these numbers]` |
| `$Extend\$Quota`    | 18       | `:$O`, `:$Q`                                        | [§6.16](#quota)          | `[OBSERVED: src/mkfs.rs — record 16/17/18 respectively; conventional NTFS uses 24/25/26 but Windows accepts these numbers]` |
| `$Extend\$Reparse`  | 17       | `:$R`                                               | [§6.17](#reparse-index)  | `[OBSERVED: src/mkfs.rs — record 16/17/18 respectively; conventional NTFS uses 24/25/26 but Windows accepts these numbers]` |
| `$Extend\$UsnJrnl`  | (varies) | `:$Max` (resident), `:$J` (non-resident sparse)     | [§5](05-logfile-journal.md) | |

`[OBSERVED: src/mkfs.rs:1146-1202]` — `$Extend` is built as a directory with a populated `$I30` index enumerating its three children. This matches the Microsoft format.com reference structure. The earlier `chkdsk /scan = 13` ceiling was caused by missing VIEW_INDEX flags on the child records, not the directory shape itself.

### Record numbers {#extend-record-numbers}

`rust-fs-ntfs` places the three `$Extend` children at records **16**, **17**, and **18**.
The conventional NTFS 3.x assignment (often cited as 24/$Quota, 25/$ObjId, 26/$Reparse) comes from
third-party documentation and the Microsoft `format.com` reference formatter; it is not required by the
Windows driver — chkdsk accepts any record numbers for these files provided the `$I30` parent index
and VIEW_INDEX flags are correct `[OBSERVED: 42/42 chkdsk scenarios pass with records 16/17/18;
src/mkfs.rs:169-171]`.

### `$UsnJrnl` cross-link

The USN change journal lives under `$Extend` but is fully covered in
[§5 $LogFile & journal](05-logfile-journal.md#usn-journal). Only its
location (under `$Extend`) is mentioned here.

---

## $ObjId (record 16 in rust-fs-ntfs, conventional 25) {#objid}

`$Extend\$ObjId` indexes per-file Object IDs. Each file that has been
assigned an Object ID (via `FSCTL_CREATE_OR_GET_OBJECT_ID`) carries an
`$OBJECT_ID` (type `0x40`) attribute on its MFT record, *and* an
entry in `$Extend\$ObjId:$O` mapping the GUID to the file's MFT
reference.

### `$OBJECT_ID` attribute layout {#objid-attr}

```
Offset  Size  Field             Description
------  ----  ----------------  -----------------------------------------
0x00    16    ObjectId          Per-file GUID (the canonical identity)
0x10    16    BirthVolumeId     GUID of the volume the file was created
                                on. Optional.
0x20    16    BirthObjectId     ObjectId at creation time. Optional.
0x30    16    DomainId          Active Directory domain GUID. Optional.
```

`[UNVERIFIED]` against `[MS-NTFS]` for the optional-fields rule —
this is the conventional public layout. The
`fs_ntfs_read_object_id` C ABI in `rust-fs-ntfs` returns the first
16 bytes (the canonical `ObjectId`) `[OBSERVED: docs/STATUS.md]`.

### `$O` index keying {#objid-index}

The `:$O` index is keyed by the 16-byte `ObjectId` GUID, sorted
byte-wise. The value (the index entry's data area) is the file's
8-byte MFT reference. `[UNVERIFIED]` for the precise keying — the
`$O` index entry layout is not enumerated in any authoritative source
consulted to date.

### Repair semantics

A missing `:$O` entry with a present `$OBJECT_ID` attribute is an
index inconsistency; reinsert the index entry. A `:$O` entry whose
target MFT record does not carry the matching `$OBJECT_ID` is an
orphan; delete it. `[UNVERIFIED]` — derived from the general
`$Extend` index repair pattern applied analogously.

---

## $Quota (record 18 in rust-fs-ntfs, conventional 24) {#quota}

`$Extend\$Quota` tracks per-user disk usage when quotas are enabled
on the volume. It carries two indexes:

- `:$O` — keyed by user SID; value points to a record in `:$Q`.
- `:$Q` — keyed by `Owner_Id` (32-bit user-ID surrogate). Each entry
  carries the user's quota record:

```
Offset  Size  Field              Description
------  ----  -----------------  -----------------------------------------
0x00    4     Version            Quota record version
0x04    4     Flags              QUOTA_FLAG_DEFAULT_LIMITS,
                                 QUOTA_FLAG_USER_DISABLED, etc.
0x08    8     BytesUsed          Bytes currently consumed by this user
0x10    8     ChangeTime         Last quota-record modification (NT time)
0x18    8     ThresholdLimit     Warning threshold (bytes); -1 = none
0x20    8     HardLimit          Hard ceiling (bytes); -1 = none
0x28    4     ExceededTime       Time threshold was first exceeded
                                 (NT time / 10^7 seconds; 0 if not)
0x2C    var   SID                Variable-length owner SID
```

`[UNVERIFIED]` — the layout is not enumerated in any authoritative
source consulted to date. The structure above is the conventional
public-documentation form (`[NTFSCOM: $Quota]` `[UNVERIFIED]`).

### NTFS version requirement

Quota support requires NTFS 3.0+. On NTFS 1.2 volumes the `$Quota`
file does not exist. `[UNVERIFIED]`.

### Status in `rust-fs-ntfs`

`$Quota` is created at format time as a VIEW_INDEX file at record 18 `[OBSERVED: src/mkfs.rs:1261-1319]`. Both `:$O` (SID→OwnerID) and `:$Q` (OwnerID→quota_info) indexes are built. For volumes with cluster size < 4 KiB, `:$Q` is pre-populated with a default entry for OwnerID=1. No quota enforcement at runtime.

---

## $Reparse (record 17 in rust-fs-ntfs, conventional 26) {#reparse-index}

`$Extend\$Reparse` carries a single index `:$R` that maps reparse
tags to the MFT references of files that own a reparse point with
that tag. It exists so that "find every file with reparse tag X" is
an index walk rather than a full MFT scan.

### `:$R` index keying {#reparse-index-key}

The index key is the concatenation of:

```
Offset  Size  Field                Description
------  ----  -------------------  -----------------------------------------
0x00    4     ReparseTag           The tag value (matches the MFT record's
                                   $REPARSE_POINT.ReparseTag)
0x04    8     FileReference        8-byte MFT reference (rec# + sequence)
                                   of the owning file
```

`[UNVERIFIED]` — the entry layout is not enumerated in any
authoritative source consulted to date. The form above is the
conventional public layout.

Sorting is by `(ReparseTag, FileReference)` lexicographically. A
single tag can have many entries (one per file using it).

### Cross-check with `$REPARSE_POINT` attribute {#reparse-index-crosscheck}

Per the validation rule cited in [§6.8](#reparse-validation): every
file with a `$REPARSE_POINT` attribute MUST have a corresponding
`:$R` entry. Repair: re-insert missing entries during the `$Extend`
indexes verification pass. [UNVERIFIED]

A `:$R` entry whose `FileReference` resolves to a record without a
matching `$REPARSE_POINT` (or with a different tag) is an orphan and
gets deleted.

### Status in `rust-fs-ntfs`

The writer creates `$Extend\$Reparse` at record 17 as a VIEW_INDEX file but does **not** maintain `:$R` index entries on
reparse-point write/delete. This is tracked in the open-issues list
(see `docs/mkfs-bug-catalog.md` and the open-questions file
referenced below). `[OBSERVED: docs/STATUS.md]`.

---

## References

- `[MS-FSCC §2.1.2]`, `[MS-FSCC §2.4]`, `[MS-FSCC §2.4.15]` —
  Microsoft Open Specifications. See
  [notes/references.md](../notes/references.md).
- `[MS-NTFS]`, `[MS-DTYP §2.4.6]` — Microsoft Open Specifications.
- `[OBSERVED: src/ea_io.rs]` — EA encoder / decoder.
- `[OBSERVED: src/upcase.rs]` — `$UpCase` reader + canonical-blob
  comment.
- `[OBSERVED: src/upcase-canonical.bin]` — captured 128 KiB Microsoft
  reference blob.
- `[OBSERVED: src/record_build.rs]` — `$REPARSE_POINT` and named
  `$DATA` writers.
- `[OBSERVED: docs/STATUS.md]` — C ABI surface and per-feature
  implementation status.
- `[OBSERVED: docs/chkdsk-improvement-findings.md §2.6]` — upcase mismatch diagnostic,
  CJK label byte verification, `$Extend` rec 11 structural note.
- `[OBSERVED: docs/mkfs-bug-catalog.md]` — `$FILE_NAME` mirror sizes,
  `frs.cxx 0x60f` outstanding chkdsk error.

## Open questions

Section-local `[UNVERIFIED]` claims for §6 are tracked in
[notes/open-questions.md → §6 Special streams](../notes/open-questions.md#6-special-streams).

[← Prev: $LogFile & journal](05-logfile-journal.md) | [TOC](../ntfs-specification.md)
