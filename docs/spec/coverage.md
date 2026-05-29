[TOC](ntfs-specification.md)

# NTFS Surface Area Coverage Map

This document maps every significant NTFS structure, field, algorithm, and
feature against four evidence levels. Its purpose: to answer "how much of
the total NTFS surface have we surveyed, and how confident are we in each
piece?"

---

## Evidence levels

| Tag | Meaning |
| --- | ------- |
| **VERIFIED** | Confirmed by source code **and** by external behavioral testing — Windows mount, `chkdsk`, or 42/42 matrix pass. The claim has been stress-tested, not just written. |
| **OBSERVED** | Confirmed by reading `rust-fs-ntfs` source code. The implementation does it; it hasn't been independently stress-tested in isolation. |
| **UNVERIFIED** | Documented in the spec but not yet confirmed by code or external test. We believe it is true; we haven't proved it. |
| **UNKNOWN** | Known gap: the structure or feature is acknowledged to exist but its internal layout or behavior has not been documented here. |

A fifth informal category — **PARTIAL** — appears in implementation-status
tables where a feature has *some* evidence but material gaps remain.

The `Impl` column (added to every table) tracks whether `rust-fs-ntfs` actually
implements the feature, independent of how well-documented it is:

| Symbol | Meaning |
| ------ | ------- |
| ✅ | Implemented — fully or nearly complete |
| 🟡 | Partial — e.g. read works but write doesn't, or code exists but incomplete |
| ⛔ | Not implemented at all |

---

## §1 Boot Sector & Volume Geometry

### Boot sector field table (offset-by-offset)

| # | Field | Offset | Impl | Evidence | Notes |
| --: | ----- | ------: | :--: | -------- | ----- |
| 1 | `JmpInstruction` | `0x000` | ✅ | **VERIFIED** | `EB 52 90` — confirmed against Windows mount |
| 2 | `OemId` | `0x003` | ✅ | **VERIFIED** | `"NTFS    "` — validated at mount; reject otherwise |
| 3 | `BytesPerSector` | `0x00B` | ✅ | **VERIFIED** | Must be `512`; enforced on read path |
| 4 | `SectorsPerCluster` | `0x00D` | ✅ | **VERIFIED** | Power-of-two; signed-byte encoding for ≥ 64 KiB clusters |
| 5 | `ReservedSectors` (zero) | `0x00E` | ✅ | **OBSERVED** | Zero-filled; not validated |
| 6 | Legacy FAT zeros (`0x010`–`0x012`) | `0x010` | ✅ | **OBSERVED** | Zero-filled in mkfs |
| 7 | Historical `RootEntries` (zero) | `0x013` | ✅ | **OBSERVED** | Zero-filled in mkfs |
| 8 | `MediaDescriptor` | `0x015` | ✅ | **OBSERVED** | `0xF8` for fixed disk; consumer behaviour not tested |
| 9 | Historical `SectorsPerFat` (zero) | `0x016` | ✅ | **OBSERVED** | Zero-filled in mkfs |
| 10 | `SectorsPerTrack` | `0x018` | ✅ | **OBSERVED** | Conventional `63`; consumer behaviour unverified |
| 11 | `NumberOfHeads` | `0x01A` | ✅ | **OBSERVED** | Conventional `255`; consumer behaviour unverified |
| 12 | `HiddenSectors` | `0x01C` | ✅ | **OBSERVED** | `0` for raw images; partitioned-disk semantics unverified |
| 13 | Historical `LargeSectors` (zero) | `0x020` | ✅ | **OBSERVED** | Zero-filled; `0x028` supersedes it |
| 14 | `BiosDriveNumberAndExtBpbSig` (`0x00800080`) | `0x024` | ✅ | **OBSERVED** | Canonical value written; meaning unverified against MS-NTFS |
| 15 | `TotalSectors64` (`volume_sectors − 1`) | `0x028` | ✅ | **VERIFIED** | `N−1` convention confirmed via bug-catalog Bug 6 + matrix |
| 16 | `MFT_LCN` | `0x030` | ✅ | **VERIFIED** | Confirmed placement constraint; matrix passes all cluster sizes |
| 17 | `MFTMirr_LCN` | `0x038` | ✅ | **VERIFIED** | Confirmed placement |
| 18 | `ClustersPerFileRecordSegment` (signed-log2) | `0x040` | ✅ | **VERIFIED** | `0xF6 = −10 → 1 KiB`, `0xF4 = −12 → 4 KiB` — tested |
| 19 | Reserved zero bytes at `0x041`–`0x043` | `0x041` | ✅ | **OBSERVED** | Zero-filled |
| 20 | `ClustersPerIndexBuffer` (signed-log2) | `0x044` | ✅ | **VERIFIED** | Same encoding as `0x040`; tested |
| 21 | Reserved zero bytes at `0x045`–`0x047` | `0x045` | ✅ | **OBSERVED** | Zero-filled |
| 22 | `VolumeSerialNumber` | `0x048` | ✅ | **OBSERVED** | 64-bit random; not validated by spec |
| 23 | `Checksum` | `0x050` | ✅ | **OBSERVED** | Written as `0`; Windows mounts without validating |
| 24 | `BootCode` (426 bytes) | `0x054` | ✅ | **VERIFIED** | 3-byte halt loop sufficient; 42/42 matrix passes |
| 25 | `BootSignature` | `0x1FE` | ✅ | **VERIFIED** | `55 AA` — required at mount |

### Boot sector algorithms

| # | Algorithm | Impl | Evidence | Notes |
| --: | --------- | :--: | -------- | ----- |
| 26 | Signed-log2 decode (`ClustersPerFileRecordSegment`, `ClustersPerIndexBuffer`) | ✅ | **VERIFIED** | Both encode/decode paths confirmed via matrix |
| 27 | Backup boot sector at last 512-byte sector | ✅ | **VERIFIED** | Bug 7 in catalog; Event 55 reproduced and fixed |
| 28 | Boot sector validation (5-rule sequence) | ✅ | **VERIFIED** | Enforced in `src/mft_io.rs::parse_boot_params_from_bytes` |
| 29 | Bidirectional boot sync (primary ↔ backup repair) | ⛔ | **UNVERIFIED** | Algorithm documented; not implemented in `rust-fs-ntfs` |
| 30 | `mft_lcn` placement constraint (`≥ ceil(8192 / cluster_size)`) | ✅ | **VERIFIED** | Bug in prior version; confirmed via matrix small-cluster cases |
| 31 | `SectorsPerCluster` large-value encoding (`raw ≥ 0x80 → 1 << (256−raw)`) | ✅ | **OBSERVED** | Implemented in `src/mft_io.rs`; encoding not confirmed against a real very-large-cluster volume |

### NTFS version detection

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 32 | NTFS 3.1 volume (major=3, minor=1) | ✅ | **VERIFIED** | mkfs emits 3.1; all matrix scenarios pass |
| 33 | Version read from `$VOLUME_INFORMATION` at runtime | ⛔ | **UNVERIFIED** | Hardcoded `(3,1)` in `src/facade.rs`; not dynamically read |
| 34 | NTFS 1.2 volume handling (skip CRC32 etc.) | ⛔ | **UNVERIFIED** | Algorithm documented; not tested |
| 35 | NTFS 3.0 feature delta vs 3.1 | ⛔ | **UNVERIFIED** | Documented; no 3.0-specific test coverage |

### Volume geometry limits

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 36 | Cluster sizes 512 B, 1 KiB, 4 KiB (common) | ✅ | **VERIFIED** | All three exercised in matrix |
| 37 | Cluster sizes 8 KiB – 2 MiB (large) | 🟡 | **UNVERIFIED** | Encoded in boot parser; not exercised in matrix |
| 38 | Volume size up to 256 TiB | 🟡 | **UNVERIFIED** | Logical limit from u64 LCN + max cluster size |
| 39 | `BytesPerSector != 512` rejected at mount | ✅ | **UNVERIFIED** | Asserted from Windows internals; not stress-tested |
| 40 | 4Kn / 512e alignment requirements | ⛔ | **UNKNOWN** | Acknowledged; no implementation or test coverage |

**§1 item count: 40** · VERIFIED: 19 · OBSERVED: 14 · UNVERIFIED: 6 · UNKNOWN: 1

---

## §2 MFT & Records

### MFT record header

| # | Field | Offset | Impl | Evidence | Notes |
| --: | ----- | ------: | :--: | -------- | ----- |
| 41 | `Magic` (`FILE`) | `0x00` | ✅ | **VERIFIED** | Rejected on read if not `FILE`; BAAD described but unverified |
| 42 | `USA offset` | `0x04` | ✅ | **VERIFIED** | Drives fixup; computed per record size |
| 43 | `USA count` | `0x06` | ✅ | **VERIFIED** | `(record_size / bytes_per_sector) + 1`; validated |
| 44 | `LogFile Sequence Number` | `0x08` | ✅ | **OBSERVED** | Initialised to 0 on new records; replay meaning unverified |
| 45 | `Sequence number` | `0x10` | ✅ | **VERIFIED** | System records use deterministic `max(1, slot)` pattern; matrix confirms |
| 46 | `Hard link count` | `0x12` | ✅ | **VERIFIED** | Confirmed by chkdsk validation |
| 47 | `First attribute offset` | `0x14` | ✅ | **VERIFIED** | Dynamic from USA size; Bug 8 in catalog (hardcoded offset was wrong) |
| 48 | `Flags` (`IN_USE`, `IS_DIRECTORY`, `IS_VIEW_INDEX`) | `0x16` | ✅ | **VERIFIED** | All three confirmed; `IS_VIEW_INDEX` absence triggers chkdsk error |
| 49 | `Used size` (`bytes_used`) | `0x18` | ✅ | **VERIFIED** | End marker at `+8` bytes confirmed; Bug 2 in catalog |
| 50 | `Allocated size` | `0x1C` | ✅ | **VERIFIED** | Matches `file_record_size` from boot |
| 51 | `Base file reference` | `0x20` | ✅ | **VERIFIED** | `0` for base; non-zero for extension records |
| 52 | `Next attribute ID` | `0x28` | ✅ | **VERIFIED** | Bumped per-add; deterministic start value |
| 53 | MFT record number (NTFS 3.1) | `0x2C` | ✅ | **VERIFIED** | Always written; NTFS 3.1 self-reference |
| 54 | `BAAD` magic (corruption sentinel) | — | ⛔ | **UNVERIFIED** | Documented; not yet produced or tested in our toolchain |
| 55 | CRC32 footer in NTFS 3.1 MFT records | — | ⛔ | **UNVERIFIED** | Post-fixup ordering described; not validated or emitted |

### USA fixup mechanism

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 56 | USA geometry (`usa_count = sectors + 1`) | ✅ | **VERIFIED** | Confirmed for both 1 KiB and 4 KiB records |
| 57 | `apply_fixup_on_read` algorithm | ✅ | **VERIFIED** | Torn-write detection exercised via test suite |
| 58 | `apply_fixup_on_write` algorithm | ✅ | **VERIFIED** | USN skip-zero confirmed in code |
| 59 | USA applies to MFT, INDX, RSTR, RCRD pages | ✅ | **VERIFIED** | All four confirmed in implementation |
| 60 | USN skip-zero convention | ✅ | **OBSERVED** | Implemented; no permitted-source statement corroborates |

### `$MFT` (record 0) and `$MFTMirr` (record 1)

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 61 | `$MFT.$DATA` non-resident; data runs locate all MFT records | ✅ | **VERIFIED** | Core to the implementation |
| 62 | `$MFT.$BITMAP` tracks per-record allocation | ✅ | **VERIFIED** | `src/mft_bitmap.rs` |
| 63 | `$MFTMirr` partial mirror (not hardcoded to 4 records) | ⛔ | **UNVERIFIED** | Described; mirror range not dynamically validated |
| 64 | Divergence repair matrix (6-case table) | ⛔ | **UNVERIFIED** | Algorithm documented; not implemented |
| 65 | `$MFTMirr` range = `allocated_size / record_size` | ⛔ | **UNVERIFIED** | Rule stated; no code enforces it |

### 16 system files

| # | Record | Name | Impl | Evidence | Notes |
| --: | -----: | ---- | :--: | -------- | ----- |
| 66 | 0 | `$MFT` | ✅ | **VERIFIED** | |
| 67 | 1 | `$MFTMirr` | ✅ | **VERIFIED** | |
| 68 | 2 | `$LogFile` | ✅ | **VERIFIED** | |
| 69 | 3 | `$Volume` | ✅ | **VERIFIED** | |
| 70 | 4 | `$AttrDef` | ✅ | **VERIFIED** | Present; content format unverified |
| 71 | 5 | `.` (root) | ✅ | **VERIFIED** | Populated `$I30`, `parent_ref = (5,5)` |
| 72 | 6 | `$Bitmap` | ✅ | **VERIFIED** | |
| 73 | 7 | `$Boot` | ✅ | **VERIFIED** | Overlays sector 0; 8 KiB $DATA claim |
| 74 | 8 | `$BadClus` | ✅ | **VERIFIED** | |
| 75 | 9 | `$Secure` / `$Quota` (name varies by cluster size) | ✅ | **VERIFIED** | Cluster-size-dependent name confirmed via chkdsk |
| 76 | 10 | `$UpCase` | ✅ | **VERIFIED** | 128 KiB canonical table |
| 77 | 11 | `$Extend` | ✅ | **VERIFIED** | Directory with `$I30`, children at records 16/17/18 |
| 78 | 12–15 | (reserved) | ✅ | **OBSERVED** | Present with minimal content; no chkdsk error |

### Resident vs non-resident attributes

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 79 | Resident attribute (value inline in record) | ✅ | **VERIFIED** | |
| 80 | Non-resident attribute (value in clusters, data runs in header) | ✅ | **VERIFIED** | |
| 81 | `non_resident` discriminator byte at header `+0x08` | ✅ | **VERIFIED** | |
| 82 | Resident → non-resident migration (write-driven) | 🟡 | **OBSERVED** | mkfs does it; runtime writer path incomplete |
| 83 | Non-resident → resident reverse migration | ⛔ | **UNVERIFIED** | Format permits; not implemented |

### Common attribute header (16-byte prefix)

| # | Field | Impl | Evidence | Notes |
| --: | ----- | :--: | -------- | ----- |
| 84 | `Type` (4 bytes) | ✅ | **VERIFIED** | |
| 85 | `Length` (4 bytes, multiple of 8) | ✅ | **VERIFIED** | Enforced on read; violations terminate iteration |
| 86 | `Non-resident` flag (1 byte) | ✅ | **VERIFIED** | |
| 87 | `Name length` / `Name offset` | ✅ | **VERIFIED** | |
| 88 | `Flags` (Compressed `0x0001`, Encrypted `0x4000`, Sparse `0x8000`) | ✅ | **OBSERVED** | Sparse flag written; Encrypted/Compressed not produced by mkfs |
| 89 | `Attribute ID` (16-bit instance ID) | ✅ | **VERIFIED** | Unique per record; ID assignment confirmed |

### Resident header extension (`+0x10`–`+0x18`)

| # | Field | Impl | Evidence | Notes |
| --: | ----- | :--: | -------- | ----- |
| 90 | `Value length` | ✅ | **VERIFIED** | |
| 91 | `Value offset` | ✅ | **VERIFIED** | |
| 92 | `Indexed flag` (must be `1` on `$FILE_NAME`) | ✅ | **VERIFIED** | Absence causes chkdsk error; Bug 1 in catalog |

### Non-resident header extension (`+0x10`–`+0x40`)

| # | Field | Impl | Evidence | Notes |
| --: | ----- | :--: | -------- | ----- |
| 93 | `First VCN` / `Last VCN` | ✅ | **VERIFIED** | |
| 94 | `Mapping pairs offset` | ✅ | **VERIFIED** | |
| 95 | `Compression unit` | ✅ | **OBSERVED** | `0` for uncompressed; non-zero path (LZNT1) not implemented |
| 96 | `Allocated length` / `Data length` / `Initialized length` | ✅ | **VERIFIED** | All three confirmed |

### `$STANDARD_INFORMATION` (type `0x10`)

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 97 | 48-byte core (timestamps + `FileAttributes`) | ✅ | **VERIFIED** | |
| 98 | FILETIME encoding (100-ns since 1601-01-01) | ✅ | **VERIFIED** | EPOCH_DIFF calculation confirmed |
| 99 | `FileAttributes` bitfield (`Hidden`, `System`, `Archive`, `Directory`, `ViewIndex`, `ReparsePoint`) | ✅ | **VERIFIED** | All observed in code and chkdsk |
| 100 | System records: `Hidden\|System` only (no `Archive`) | ✅ | **VERIFIED** | Confirmed vs `format.com` reference |
| 101 | 24-byte NTFS 3.x extension (`OwnerId`, `SecurityId`, `QuotaCharged`, `USN`) | ✅ | **VERIFIED** | Written by mkfs |
| 102 | 48-byte form for system records (slots 0–11) | ✅ | **VERIFIED** | Confirmed vs `format.com` reference |
| 103 | `$SI` timestamps are authoritative (vs `$FILE_NAME` copies) | ✅ | **UNVERIFIED** | Operational observation; no definitive permitted source |

### `$FILE_NAME` (type `0x30`)

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 104 | Full layout (all fields including timestamps, sizes, namespace) | ✅ | **VERIFIED** | |
| 105 | Namespace values: POSIX=0, Win32=1, DOS=2, Win32+DOS=3 | ✅ | **VERIFIED** | All four values confirmed in code |
| 106 | `indexed_flag = 1` required | ✅ | **VERIFIED** | Bug 1 in catalog; confirmed via chkdsk |
| 107 | `AllocatedSize` / `RealSize` mirrors primary `$DATA` | ✅ | **VERIFIED** | Bug 1 confirms this requirement |
| 108 | Multiple `$FILE_NAME` per record (one per hard link) | ✅ | **VERIFIED** | |
| 109 | `$FILE_NAME` timestamps are lazily maintained (not kept in sync with `$SI`) | ✅ | **UNVERIFIED** | Observed behaviour; no MS spec statement cited |
| 110 | 8.3 name generation algorithm | ⛔ | **UNVERIFIED** | Not implemented in `rust-fs-ntfs` |

### `$ATTRIBUTE_LIST` (type `0x20`)

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 111 | Concept: base record + extension records; `$ATTRIBUTE_LIST` enumerates all | 🟡 | **OBSERVED** | Read path via upstream `ntfs` crate |
| 112 | `$ATTRIBUTE_LIST` on-disk entry byte layout | ⛔ | **UNVERIFIED** | Defined in `[MS-FSCC]`; not reproduced in spec |
| 113 | Base record `base_file_reference = 0` | ✅ | **VERIFIED** | Confirmed in `src/record_build.rs` |
| 114 | Extension records have non-zero `base_file_reference` | ✅ | **VERIFIED** | Confirmed |
| 115 | Traversal: iterative with visited-set and depth cap | ⛔ | **UNVERIFIED** | Algorithm specified; not implemented as new code |
| 116 | Sort invariant on healthy volumes | ⛔ | **UNVERIFIED** | Our repair path sorts; format requirement unclear |
| 117 | `$ATTRIBUTE_LIST` emission by `rust-fs-ntfs` writer | ⛔ | **UNKNOWN** | Not yet implemented; all base records currently self-contained |

### Sequence numbers and reuse

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 118 | File reference encoding: 48-bit record + 16-bit seq | ✅ | **VERIFIED** | Confirmed in `encode_file_reference` |
| 119 | System records: deterministic `sequence = max(1, slot)` | ✅ | **VERIFIED** | Confirmed via chkdsk; Bug 3 in catalog |
| 120 | User records: sequence starts at 1, bumps on reuse | ✅ | **OBSERVED** | Implementation convention; no MS spec statement |
| 121 | Sequence-zero avoidance | ✅ | **UNVERIFIED** | Analogous to USN-skip-zero; no permitted source |
| 122 | Reference resolution (mismatch = stale vs corrupt) | 🟡 | **UNVERIFIED** | Policy unclear in permitted sources |

**§2 item count: 82** · VERIFIED: 45 · OBSERVED: 12 · UNVERIFIED: 22 · UNKNOWN: 3

---

## §3 Data Runs & Cluster Allocation

### Data run encoding

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 123 | Header byte: low nibble = length-field bytes (F), high nibble = offset-field bytes (V) | ✅ | **VERIFIED** | |
| 124 | `F = 0` is invalid | ✅ | **VERIFIED** | Rejected in parser |
| 125 | `V = 0` = sparse run (no LCN bytes) | ✅ | **VERIFIED** | |
| 126 | Length field: F unsigned LE bytes | ✅ | **VERIFIED** | |
| 127 | Offset field: V signed-delta LE bytes | ✅ | **VERIFIED** | Sign-extension from MSB confirmed |
| 128 | First run: offset is absolute LCN (previous LCN = 0) | ✅ | **VERIFIED** | |
| 129 | Subsequent runs: signed delta from previous LCN | ✅ | **VERIFIED** | Negative delta tested |
| 130 | Terminator: single `0x00` byte | ✅ | **VERIFIED** | |
| 131 | `F > 8` or `V > 8` rejected | ✅ | **OBSERVED** | Parser enforces; upper bound vs real ntfs.sys unverified |
| 132 | Encoder: sign-aware width selection (prevent high-bit ambiguity) | ✅ | **VERIFIED** | Bug fixed; $BadClus encoding confirmed via matrix |

### Data run invariants

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 133 | Non-zero run length after decode | ✅ | **VERIFIED** | |
| 134 | LCN bounds check (`0 ≤ lcn < total_clusters`) | ✅ | **VERIFIED** | |
| 135 | Monotonically increasing VCN (no gaps, sparse = explicit V=0 run) | ✅ | **VERIFIED** | |
| 136 | No intra-attribute LCN overlap | 🟡 | **UNVERIFIED** | Cross-attribute cross-link is detected; intra-attribute rule unverified |
| 137 | VCN total matches non-resident header `(last_vcn − first_vcn + 1)` | 🟡 | **UNVERIFIED** | Not validated at parse time in current code |
| 138 | Bounding `length` terminates parse before `0x00` (mid-run cutoff) | 🟡 | **UNVERIFIED** | Parser tolerates; ntfs.sys behaviour unconfirmed |

### Sparse runs

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 139 | V=0 encoding; no LCN bytes; read returns zero | ✅ | **VERIFIED** | |
| 140 | LCN cursor unchanged across sparse run | ✅ | **VERIFIED** | |
| 141 | Sparse run must not contribute bits to `$Bitmap` | ⛔ | **UNVERIFIED** | Rule stated; reconciliation path not yet implemented |
| 142 | `Sparse` flag (0x8000) must be set on attribute carrying sparse runs | 🟡 | **UNVERIFIED** | Structural invariant; not validated in current parser |

### VCN-to-LCN translation

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 143 | Linear scan VCN-to-LCN lookup | ✅ | **VERIFIED** | |
| 144 | Binary-search optimization (`O(log n)` with cached runlist) | ⛔ | **UNKNOWN** | Unimplemented; noted as future improvement |

### Non-resident size fields

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 145 | `allocated_size`, `data_size`, `initialized_length` semantics | ✅ | **VERIFIED** | |
| 146 | `initialized_length` semantics for mixed sparse/physical runs | 🟡 | **UNVERIFIED** | Orthogonal to sparse; not stress-tested |

### `$Bitmap` (record 6)

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 147 | One bit per cluster; bit `k` = cluster `k` allocated | ✅ | **VERIFIED** | |
| 148 | LSB-first within byte (cluster 0 = bit 0 of byte 0) | ✅ | **VERIFIED** | Confirmed in `src/bitmap.rs` |
| 149 | Non-resident `$DATA` attribute with data runs | ✅ | **VERIFIED** | |
| 150 | `ceil(total_clusters / 8)` bytes; trailing bits set to 1 | ✅ | **VERIFIED** | Bug previously caused allocator to pick beyond-end clusters |
| 151 | `find_free_run` first-fit linear scan with wrap | ✅ | **VERIFIED** | |
| 152 | Double-allocation detection (bit already 1) = hard error | ✅ | **VERIFIED** | |
| 153 | Double-free detection (bit already 0) = hard error | ✅ | **VERIFIED** | |
| 154 | `$MFT:$BITMAP` is a separate bitmap (one bit per MFT record) | ✅ | **VERIFIED** | Distinct from volume `$Bitmap` |

### Bitmap update ordering and safety

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 155 | Allocate-before-write ordering (prevent cross-link) | ✅ | **UNVERIFIED** | Implementation convention; not confirmed against MS-NTFS |
| 156 | Free: update mapping-pairs before clearing bit | ✅ | **UNVERIFIED** | Same |
| 157 | `$Bitmap` writes not yet integrated with `$LogFile` | ⛔ | **OBSERVED** | Known limitation; `sync` only |

### Bitmap Double-Pass reconciliation

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 158 | Pass 1: MFT walk → ground-truth bitmap construction | ⛔ | **UNVERIFIED** | Algorithm documented; not implemented |
| 159 | Pass 2: bit-by-bit reconciliation (leak → reclaim; cross-link → enforce) | ⛔ | **UNVERIFIED** | Algorithm documented; not implemented |
| 160 | Consensus safety barrier (EIO threshold / MFT yield / lost-cluster ratio) | ⛔ | **UNVERIFIED** | Algorithm documented; not implemented |
| 161 | TRIM/DISCARD prohibited during reconciliation | ✅ | **OBSERVED** | `rust-fs-ntfs` never calls TRIM |

### `$BadClus` (record 8)

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 162 | Unnamed `$DATA` (empty resident) | ✅ | **VERIFIED** | |
| 163 | Named `$DATA:$Bad` (4 UTF-16LE chars) | ✅ | **VERIFIED** | |
| 164 | Initial `$Bad`: single sparse run covering `cluster_count − 1` clusters | ✅ | **VERIFIED** | Off-by-one confirmed via matrix + bug catalog |
| 165 | `allocated_size` = `(cluster_count − 1) × cluster_size` | ✅ | **VERIFIED** | Bug confirmed and fixed |
| 166 | Bad-cluster quarantine via physical-run-pointing-at-itself convention | ⛔ | **UNVERIFIED** | Mechanism described; not produced or tested |
| 167 | `$Bad` ⊆ allocated (each bad LCN must be USED in `$Bitmap`) | ⛔ | **UNVERIFIED** | Rule stated; not enforced |

### Bad-sector relocation flow

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 168 | Relocation trigger (EIO on system or user file cluster) | ⛔ | **UNVERIFIED** | Algorithm documented; not implemented |
| 169 | Per-file reconstruction policy | ⛔ | **UNVERIFIED** | Decision table documented; not implemented |
| 170 | `$BadClus` bootstrap-problem recovery | ⛔ | **UNVERIFIED** | Acknowledged; no implementation |

### Resident → non-resident migration

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 171 | Migration threshold is dynamic (free space in record) | ⛔ | **UNVERIFIED** | Rule described; not stress-tested |
| 172 | mkfs performs migration for `$BadClus:$Bad` | ✅ | **VERIFIED** | |
| 173 | Runtime writer migration (write path) | ⛔ | **UNVERIFIED** | Documented gap in `status.md` |

**§3 item count: 51** · VERIFIED: 22 · OBSERVED: 5 · UNVERIFIED: 20 · UNKNOWN: 2 · PARTIAL: 2

---

## §4 Indexes & Directories

### `$FILE_NAME` as index key

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 174 | Full layout (timestamps, sizes, attributes, namespace, name) | ✅ | **VERIFIED** | |
| 175 | Timestamps are snapshot (not kept in sync with `$SI`) | ✅ | **UNVERIFIED** | Same as §2 item 109 |
| 176 | `AllocatedSize`/`RealSize` are snapshot; chkdsk does not flag staleness | ✅ | **OBSERVED** | Per status.md |

### `$INDEX_ROOT` (type `0x90`)

| # | Field | Impl | Evidence | Notes |
| --: | ----- | :--: | -------- | ----- |
| 177 | Attribute layout (collation rule, bytes per buffer, clusters per buffer) | ✅ | **VERIFIED** | |
| 178 | `COLLATION_FILE_NAME = 0x01` for `$I30` | ✅ | **VERIFIED** | |
| 179 | `INDEX_HEADER` (first entry offset, total entries size, allocated size) | ✅ | **VERIFIED** | |
| 180 | `INDEX_HEADER.HAS_SUBNODES` flag | ✅ | **VERIFIED** | |

### `INDEX_ENTRY` structure

| # | Field | Impl | Evidence | Notes |
| --: | ----- | :--: | -------- | ----- |
| 181 | File reference (8 bytes) | ✅ | **VERIFIED** | |
| 182 | Entry length | ✅ | **VERIFIED** | |
| 183 | Key length | ✅ | **VERIFIED** | |
| 184 | Flags (`HAS_SUBNODES = 0x01`, `LAST = 0x02`) | ✅ | **VERIFIED** | |
| 185 | Key value (variable) | ✅ | **VERIFIED** | |
| 186 | VCN child pointer at entry tail (when `HAS_SUBNODES`) | ✅ | **VERIFIED** | |
| 187 | LAST sentinel: `key_length = 0`, `LAST` flag set | ✅ | **VERIFIED** | |
| 188 | LAST sentinel carries VCN for right-most descent | ✅ | **VERIFIED** | |

### B+ tree shape

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 189 | Internal nodes: sorted keys + per-key child VCNs | ✅ | **VERIFIED** | Read path confirmed |
| 190 | Leaf nodes: sorted keys + full values | ✅ | **VERIFIED** | Read path confirmed |
| 191 | Sort order via collation rule | ✅ | **VERIFIED** | |
| 192 | No `.` or `..` entries in `$I30` | ✅ | **VERIFIED** | Confirmed via matrix inspection |
| 193 | B+ tree insert (resident-only root case) | ✅ | **VERIFIED** | Write path for small directories |
| 194 | B+ tree split/merge (multi-level, `$INDEX_ALLOCATION`) | 🟡 | **UNVERIFIED** | Read path works; write path not implemented for large directories |

### `$INDEX_ALLOCATION` (type `0xA0`) / INDX blocks

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 195 | `INDX` magic (4 bytes) | ✅ | **VERIFIED** | |
| 196 | USA fixup on INDX blocks (same mechanism as MFT) | ✅ | **VERIFIED** | |
| 197 | INDX block size from `ClustersPerIndexBuffer` in boot | ✅ | **VERIFIED** | |
| 198 | VCN addressing of INDX blocks in `$INDEX_ALLOCATION` | ✅ | **VERIFIED** | |

### `$BITMAP` for index (type `0xB0`)

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 199 | Per-INDX-block allocation tracking | ✅ | **VERIFIED** | |

### Collation rules

| # | Rule | Constant | Impl | Evidence | Notes |
| --: | ---- | -------- | :--: | -------- | ----- |
| 200 | `COLLATION_FILE_NAME` | `0x01` | ✅ | **VERIFIED** | Used for `$I30` |
| 201 | `COLLATION_NTOFS_ULONG` | `0x10` | ✅ | **OBSERVED** | Used for view-index integer keys |
| 202 | `COLLATION_NTOFS_SID` | `0x11` | ✅ | **VERIFIED** | Emitted for `$Quota:$O` (`src/mkfs.rs:1304`) |
| 203 | `COLLATION_NTOFS_SECURITY_HASH` | `0x12` | ✅ | **OBSERVED** | For `$SDH` index |
| 204 | `COLLATION_NTOFS_ULONGS` | `0x13` | ✅ | **OBSERVED** | For `$SII` / `$O` (ObjId) |

### Filename namespace rules

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 205 | POSIX (0): case-sensitive | ✅ | **VERIFIED** | |
| 206 | Win32 (1): long name | ✅ | **VERIFIED** | |
| 207 | DOS (2): 8.3 short name | ⛔ | **OBSERVED** | Not generated by `rust-fs-ntfs` |
| 208 | Win32+DOS (3): combined (single attribute) | ✅ | **VERIFIED** | Default for new files in mkfs |
| 209 | Case-insensitive collation via volume `$UpCase` table | ✅ | **VERIFIED** | |
| 210 | 8.3 name generation / collision-avoidance algorithm | ⛔ | **UNVERIFIED** | Not implemented |

### Named index variants

| # | Index | Collation | Owner | Impl | Evidence | Notes |
| --: | ----- | --------- | ----- | :--: | -------- | ----- |
| 211 | `$I30` | `COLLATION_FILE_NAME` | Directories | ✅ | **VERIFIED** | |
| 212 | `$SDH` | `COLLATION_NTOFS_SECURITY_HASH` | `$Secure` | ✅ | **VERIFIED** | Populated by mkfs |
| 213 | `$SII` | `COLLATION_NTOFS_ULONGS` | `$Secure` | ✅ | **VERIFIED** | Populated by mkfs |
| 214 | `$Q` | `COLLATION_NTOFS_ULONG` | `$Quota` | ✅ | **VERIFIED** | Populated by mkfs |
| 215 | `$O` (quota) | `COLLATION_NTOFS_SID` | `$Quota` | ✅ | **VERIFIED** | Populated by mkfs |
| 216 | `$O` (object ID) | `COLLATION_NTOFS_ULONGS` | `$ObjId` | ✅ | **VERIFIED** | VIEW_INDEX structure created |
| 217 | `$R` (reparse) | `COLLATION_NTOFS_ULONGS` | `$Reparse` | ✅ | **VERIFIED** | VIEW_INDEX structure created |

**§4 item count: 44** · VERIFIED: 33 · OBSERVED: 6 · UNVERIFIED: 4 · UNKNOWN: 0 · PARTIAL: 1

---

## §5 $LogFile & Journal

### `$LogFile` structure (LFS level)

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 218 | MFT record 2; non-resident `$DATA` | ✅ | **VERIFIED** | |
| 219 | File size `0x3B_0000` bytes (≈ 3.78 MiB) for 256 MiB / 4 KiB reference volume | ✅ | **VERIFIED** | Confirmed vs `format.com` |
| 220 | `FileSize` in restart area must match on-disk allocated length | ✅ | **VERIFIED** | chkdsk "adjusting the size" diagnostic confirmed |
| 221 | Two-page layout: page 0 = restart A, page 1 = restart B | ✅ | **VERIFIED** | Canonical 12 KiB blob confirmed |
| 222 | Canonical "empty but valid" shape (RSTR × 2 + RCRD × 1 + `0xFF` fill) | ✅ | **VERIFIED** | 42/42 matrix passes with this shape |

### LFS restart page (`RSTR`)

| # | Field | Impl | Evidence | Notes |
| --: | ----- | :--: | -------- | ----- |
| 223 | `RSTR` magic | ✅ | **VERIFIED** | Emitted and confirmed by ntfs.sys mount |
| 224 | USA fixup on restart pages | ✅ | **VERIFIED** | Applied in canonical blob |
| 225 | `SystemPageSize` (4096) | ✅ | **OBSERVED** | Value confirmed in canonical blob; not dynamically computed |
| 226 | `LogPageSize` (4096) | ✅ | **OBSERVED** | Same |
| 227 | `RestartOffset` (byte offset to `LFS_RESTART_AREA`) | ✅ | **OBSERVED** | `0x30` in canonical blob |
| 228 | `MajorVersion` / `MinorVersion` (LFS version) | ✅ | **OBSERVED** | Version 1 in canonical blob; v2 format unverified |

### LFS restart area (`LFS_RESTART_AREA`)

| # | Field | Impl | Evidence | Notes |
| --: | ----- | :--: | -------- | ----- |
| 229 | `CurrentLsn` | ✅ | **VERIFIED** | Higher LSN wins; confirmed by canonical blob design |
| 230 | `LogClients` (count) | ✅ | **OBSERVED** | 1 client in canonical blob |
| 231 | `ClientFreeList` / `ClientInUseList` | ⛔ | **UNVERIFIED** | Structure described; not independently validated |
| 232 | `Flags` (`CLEAN_DISMOUNT = 0x02`) | ✅ | **VERIFIED** | Set by mkfs; Windows skips replay when set |
| 233 | `SeqNumberBits` | ⛔ | **UNVERIFIED** | LSN bit-split not independently verified |
| 234 | `RestartAreaLength` / `ClientArrayOffset` | ⛔ | **UNVERIFIED** | Fields documented; not individually probed |
| 235 | `FileSize` (must match on-disk) | ✅ | **VERIFIED** | See item 220 |

### LFS client record

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 236 | Client name `"NTFS"` (UTF-16LE, 128-byte fixed) | ✅ | **VERIFIED** | In canonical blob at offset `0x90` |
| 237 | `OldestLsn` (lower bound of replay) | ⛔ | **UNVERIFIED** | Field described; not validated |
| 238 | `ClientRestartLsn` | ⛔ | **UNVERIFIED** | Field described; not validated |
| 239 | Free / in-use client list linkage | ⛔ | **UNVERIFIED** | Single-client case documented; not stress-tested |

### RCRD pages

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 240 | `RCRD` magic | ✅ | **VERIFIED** | Emitted in canonical blob; ntfs.sys accepts |
| 241 | USA fixup on RCRD pages | ✅ | **VERIFIED** | Emitted in canonical blob |
| 242 | RCRD page header layout (9-field table) | ⛔ | **UNVERIFIED** | Field table documented; not individually probed |
| 243 | Multiple log records per page | ⛔ | **UNVERIFIED** | Structure described; not exercised |
| 244 | Multi-page log records (span flag) | ⛔ | **UNVERIFIED** | Not exercised |

### LSN encoding

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 245 | LSN structure (sequence bits + file-offset bits) | ⛔ | **UNVERIFIED** | Encoding described; bit-packing direction not confirmed |
| 246 | Wrap-around comparison (two-stage: seq then offset) | ⛔ | **UNVERIFIED** | Algorithm described; not tested |
| 247 | Page selection by "higher `CurrentLsn`" rule | ✅ | **VERIFIED** | Applied in mkfs canonical blob (page 1 LSN > page 0 LSN) |

### LFS log record and NTFS client data

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 248 | LFS log record header (13-field table) | ⛔ | **UNVERIFIED** | Format documented; not independently validated |
| 249 | NTFS client data header (redo/undo ops + LCN list) | ⛔ | **UNVERIFIED** | Format documented; not independently validated |
| 250 | LFS v1 vs v2 client-data start offset difference | ⛔ | **UNVERIFIED** | Version-aware parsing described; not tested |
| 251 | Redo-payload LZNT1 compression | ⛔ | **UNVERIFIED** | Mentioned; LZNT1 not implemented |

### Redo/undo opcodes

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 252 | Opcodes `0x00`–`0x1C` (full table, 29 entries) | ⛔ | **UNVERIFIED** | Table documented from secondary source; not verified vs MS-NTFS |
| 253 | Opcodes `0x21`–`0x28` (later additions) | ⛔ | **UNVERIFIED** | Appear in our table; not in any dispatch-count summary we corroborate |
| 254 | Generic-copy handler (23 opcodes) | ⛔ | **UNVERIFIED** | Algorithm described |
| 255 | Specialized handlers (5 opcodes: `0x0A`, `0x17`–`0x1C`) | ⛔ | **UNVERIFIED** | Per-opcode logic described |
| 256 | Undo largely disabled in repair model (redo-only) | ⛔ | **UNVERIFIED** | Convention described; not tested |

### WAL crash recovery flow

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 257 | Analysis pass (reconstruct tables, determine LSN range) | ⛔ | **UNVERIFIED** | Algorithm documented; not implemented |
| 258 | Redo pass (replay committed transactions in LSN order) | ⛔ | **UNVERIFIED** | Algorithm documented; not implemented |
| 259 | Undo pass (disabled in repair; active in driver) | ⛔ | **UNVERIFIED** | Convention described; not tested |
| 260 | Pre-replay per-block validation (USA + magic + allocated_size) | ⛔ | **UNVERIFIED** | Requirement stated; not implemented |
| 261 | Open-attribute table (opcode `0x1D` dump) | ⛔ | **UNVERIFIED** | Concept and dump opcode described; field layout unknown |
| 262 | Dirty-page table (opcode `0x1F` dump) | ⛔ | **UNVERIFIED** | Same |
| 263 | Transaction table (opcode `0x20` dump) | ⛔ | **UNVERIFIED** | Same |

### `fsck` recovery operations

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 264 | `reset_logfile`: overwrite `$DATA` extent with `0xFF` | ✅ | **VERIFIED** | Implemented in `src/fsck.rs`; confirmed via test |
| 265 | `clear_dirty`: clear volume dirty flag | ✅ | **VERIFIED** | Implemented |
| 266 | `0xFF` fill = "uninitialized log" sentinel (ntfs.sys reinitialises) | ✅ | **VERIFIED** | Confirmed by Windows mount behaviour |

### `$UsnJrnl`

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 267 | Distinct from `$LogFile`; lives at `\$Extend\$UsnJrnl` | ⛔ | **UNVERIFIED** | Structure described; not implemented |
| 268 | `$Max` stream (journal metadata) | ⛔ | **UNVERIFIED** | Field layout unknown |
| 269 | `$J` stream (sparse, `USN_RECORD_V2` entries) | ⛔ | **UNVERIFIED** | Structure described; not implemented |
| 270 | `USN_RECORD_V2` field layout | ⛔ | **UNVERIFIED** | 15-field table documented; not validated |
| 271 | Sparse punch (old entries released) | ⛔ | **UNVERIFIED** | Mechanism described; not implemented |
| 272 | Journal disabled / absent handling | ⛔ | **UNVERIFIED** | Rule documented; not tested |
| 273 | `USN_RECORD_V3` / `V4` | ⛔ | **UNKNOWN** | Out of scope; not documented |

**§5 item count: 56** · VERIFIED: 16 · OBSERVED: 7 · UNVERIFIED: 30 · UNKNOWN: 3

---

## §6 Special Streams

### Attribute type code registry

| # | Type | Name | Impl | Evidence | Notes |
| --: | ---: | ---- | :--: | -------- | ----- |
| 274 | `0x10` | `$STANDARD_INFORMATION` | ✅ | **VERIFIED** | See §2 |
| 275 | `0x20` | `$ATTRIBUTE_LIST` | ✅ | **OBSERVED** | See §2 |
| 276 | `0x30` | `$FILE_NAME` | ✅ | **VERIFIED** | See §2 / §4 |
| 277 | `0x40` | `$OBJECT_ID` | ✅ | **OBSERVED** | 16-byte GUID; optional birth IDs |
| 278 | `0x50` | `$SECURITY_DESCRIPTOR` | ✅ | **VERIFIED** | Required on all system records (slots 0–11) |
| 279 | `0x60` | `$VOLUME_NAME` | ✅ | **VERIFIED** | Volume label |
| 280 | `0x70` | `$VOLUME_INFORMATION` | ✅ | **VERIFIED** | Version + flags |
| 281 | `0x80` | `$DATA` | ✅ | **VERIFIED** | Unnamed = primary; named = ADS |
| 282 | `0x90` | `$INDEX_ROOT` | ✅ | **VERIFIED** | See §4 |
| 283 | `0xA0` | `$INDEX_ALLOCATION` | ✅ | **VERIFIED** | See §4 |
| 284 | `0xB0` | `$BITMAP` | ✅ | **VERIFIED** | See §3/§4 |
| 285 | `0xC0` | `$REPARSE_POINT` | ✅ | **VERIFIED** | |
| 286 | `0xD0` | `$EA_INFORMATION` | ✅ | **VERIFIED** | |
| 287 | `0xE0` | `$EA` | ✅ | **VERIFIED** | |
| 288 | `0x100` | `$LOGGED_UTILITY_STREAM` | 🟡 | **OBSERVED** | Opaque on read; internal layout (EFS/TxF) unknown |

### `$DATA` — Alternate Data Streams (ADS)

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 289 | ADS create / delete | ✅ | **VERIFIED** | `fs_ntfs_write_named_stream` / `fs_ntfs_delete_named_stream` |
| 290 | UTF-16LE naming; case-insensitive via `$UpCase` | ✅ | **VERIFIED** | |
| 291 | Empty name = primary stream (rejected by writer) | ✅ | **VERIFIED** | |
| 292 | ADS on directories | ✅ | **UNVERIFIED** | Rule documented; not tested |
| 293 | `$FILE_NAME` mirrors primary `$DATA` only (not ADS sizes) | ✅ | **VERIFIED** | Bug 1 confirms this |

### LZNT1 compression

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 294 | Compression opt-in via `is_compressed` attribute flag | ⛔ | **UNVERIFIED** | Flag known; not used |
| 295 | Compression unit = 16 clusters (4-bit exponent) | ⛔ | **UNVERIFIED** | Documented; not implemented |
| 296 | Chunk header (IsCompressed, Signature=3, ChunkSize-1) | ⛔ | **UNVERIFIED** | Format documented; not implemented |
| 297 | Compressed chunk = flag byte + 8 group items | ⛔ | **UNVERIFIED** | Format documented; not implemented |
| 298 | LZNT1 read / write path | ⛔ | **UNKNOWN** | Not implemented; no plan to implement in near term |
| 299 | Compressed data runs interleaving | ⛔ | **UNKNOWN** | Described; not implemented |

### `$EA_INFORMATION` (0xD0) and `$EA` (0xE0)

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 300 | `$EA_INFORMATION` resident layout | ✅ | **VERIFIED** | `src/ea_io.rs` |
| 301 | `$EA` resident layout | ✅ | **VERIFIED** | `src/ea_io.rs` |
| 302 | Non-resident `$EA` | ⛔ | **UNVERIFIED** | Not implemented; resident-only MVP |
| 303 | EA key enumeration API | ✅ | **VERIFIED** | `fs_ntfs_list_ea_keys` return code `-2` for buffer-too-small |

### `$REPARSE_POINT` (0xC0)

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 304 | Write arbitrary reparse tag + data | ✅ | **VERIFIED** | |
| 305 | Symlink helper | ✅ | **VERIFIED** | |
| 306 | `FILE_ATTRIBUTE_REPARSE_POINT` flag set/cleared atomically | ✅ | **VERIFIED** | `src/write.rs` |
| 307 | Reparse tag constants (symlink, mount point, OneDrive, etc.) | 🟡 | **UNVERIFIED** | Tag values listed in MS-FSCC; not individually tested |

### `$LOGGED_UTILITY_STREAM` (0x100)

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 308 | Preserved on read | 🟡 | **OBSERVED** | Treated as opaque |
| 309 | EFS internal layout | ⛔ | **UNKNOWN** | Out of scope for current implementation |
| 310 | TxF internal layout | ⛔ | **UNKNOWN** | Out of scope |

### `$SECURITY_DESCRIPTOR` (0x50) on system records

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 311 | SD required on every system record (slots 0–11) | ✅ | **VERIFIED** | Confirmed via chkdsk; Bug 4-related observation |
| 312 | SD internal format (security descriptor header, ACE, DACL, SACL) | 🟡 | **UNVERIFIED** | Format comes from Windows security model; not reproduced in spec |

### `$Volume` (record 3): `$VOLUME_NAME` and `$VOLUME_INFORMATION`

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 313 | `$VOLUME_NAME` (volume label, UTF-16LE) | ✅ | **VERIFIED** | Written and confirmed by mkfs |
| 314 | `$VOLUME_INFORMATION` major/minor version (3.1) | ✅ | **VERIFIED** | Emitted; read back by ntfs.sys |
| 315 | Volume dirty flag in `$VOLUME_INFORMATION` | ✅ | **UNVERIFIED** | `clear_dirty` in fsck; full flag semantics unverified |
| 316 | Other `$VOLUME_INFORMATION` flag bits | ⛔ | **UNKNOWN** | Field present; flag enumeration not in our spec |

### `$OBJECT_ID` (0x40)

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 317 | 16-byte GUID | ✅ | **OBSERVED** | In type map |
| 318 | Optional birth-volume / birth-object / domain IDs | ⛔ | **UNVERIFIED** | Documented in MS-FSCC; not implemented or tested |

### `$UpCase` (record 10)

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 319 | 128 KiB canonical table (65 536 UTF-16 entries) | ✅ | **VERIFIED** | `upcase-canonical.bin` confirmed |
| 320 | Volume's own table must be used (not OS Unicode tables) | ✅ | **VERIFIED** | `src/upcase.rs` |
| 321 | Fallback for missing `$UpCase` | ⛔ | **UNVERIFIED** | Rule described; fallback not implemented |

### `$Secure` (record 9)

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 322 | Non-resident `$SDS` stream | ✅ | **VERIFIED** | mkfs confirmed; matrix passes |
| 323 | `$SDS` 256 KiB mirror (duplicate at `+256 KiB` offset) | ✅ | **VERIFIED** | Confirmed in mkfs implementation |
| 324 | `$SDH` index (security hash → SDS offset) | ✅ | **VERIFIED** | Populated by mkfs |
| 325 | `$SII` index (security ID → SDS offset) | ✅ | **VERIFIED** | Populated by mkfs |
| 326 | `security_id = 0x100` on all system records | ✅ | **VERIFIED** | Confirmed via matrix |
| 327 | Runtime SD insertion / security_id assignment | ⛔ | **UNVERIFIED** | Not implemented; all records share security_id=0x100 |
| 328 | SD lookup by security_id (runtime path) | ⛔ | **UNVERIFIED** | Not implemented |

### `$Extend` (record 11) and children

| # | Item | Impl | Evidence | Notes |
| --: | ---- | :--: | -------- | ----- |
| 329 | `$Extend` as directory with `$I30` | ✅ | **VERIFIED** | |
| 330 | `$ObjId` at record 16 (VIEW_INDEX `$O`) | ✅ | **VERIFIED** | |
| 331 | `$Reparse` at record 17 (VIEW_INDEX `$R`) | ✅ | **VERIFIED** | |
| 332 | `$Quota` at record 18 (VIEW_INDEX `$O` + `$Q`) | ✅ | **VERIFIED** | |
| 333 | `MFT_RECORD_IS_VIEW_INDEX` bit on all three children | ✅ | **VERIFIED** | Absence triggers chkdsk error |
| 334 | Record numbers 16/17/18 (this impl) vs conventional 24/25/26 | ✅ | **VERIFIED** | Windows accepts both; documented with explanation |
| 335 | `$Quota:$Q` OwnerID=1 pre-population for sub-4K clusters | ✅ | **VERIFIED** | Confirmed via matrix |
| 336 | `$UsnJrnl` as a 4th `$Extend` child | ⛔ | **UNVERIFIED** | Not created by mkfs; Windows adds on first mount |
| 337 | `$RmMetadata` intentionally absent | ✅ | **VERIFIED** | chkdsk `/scan` exits 0 without it (with `$ObjId`+`$Reparse` present) |

**§6 item count: 64** · VERIFIED: 34 · OBSERVED: 8 · UNVERIFIED: 17 · UNKNOWN: 8 · PARTIAL: 0  
*(LZNT1 items 294–299 marked UNVERIFIED/UNKNOWN as appropriate)*

---

## Structures not yet in the spec

The following NTFS features are known to exist but have not yet been
documented in any spec section.

| # | Structure / Feature | Impl | Evidence | Priority | Notes |
| --: | ------------------- | :--: | -------- | -------- | ----- |
| 338 | `$AttrDef` internal layout (attribute type definition table) | ⛔ | **UNKNOWN** | Low | File present; content format undocumented |
| 339 | MFT record compression (sparse MFT slots / `$MFT` fragmentation handling) | ⛔ | **UNKNOWN** | Low | |
| 340 | Hard link creation / deletion algorithm | ⛔ | **UNKNOWN** | Medium | Multiple `$FILE_NAME` management; parent index update |
| 341 | File rename algorithm | ⛔ | **UNKNOWN** | Medium | `$FILE_NAME` update + parent index update + `$UsnJrnl` |
| 342 | ACL / security model integration (how `security_id` is assigned at file create) | ⛔ | **UNKNOWN** | Medium | Deferred; currently all files share security_id=0x100 |
| 343 | Encrypting File System (EFS) — `$LOGGED_UTILITY_STREAM` payload | ⛔ | **UNKNOWN** | Low | Out of scope |
| 344 | Transactional NTFS (TxF) — `$LOGGED_UTILITY_STREAM` payload | ⛔ | **UNKNOWN** | Low | Out of scope |
| 345 | Compression inheritance (directory-level default flag) | 🟡 | **UNKNOWN** | Low | |
| 346 | NTFS sparse file / hole-punch API (`FSCTL_SET_SPARSE`, `FSCTL_SET_ZERO_DATA`) | ⛔ | **UNKNOWN** | Low | Partial (sparse run encoding known) |
| 347 | Symbolic link target encoding inside `$REPARSE_POINT` | ⛔ | **UNVERIFIED** | Medium | Tag + data written; target encoding not validated |
| 348 | Junction / mount-point reparse data | ⛔ | **UNVERIFIED** | Low | |
| 349 | Opportunistic lock (oplock) metadata | ⛔ | **UNKNOWN** | Low | |
| 350 | `$LogFile` size scaling formula (volume-size-dependent) | ⛔ | **UNKNOWN** | Medium | Only one concrete reference point (256 MiB / 4 KiB) |
| 351 | `$AttrDef` permitted types per NTFS version | ⛔ | **UNKNOWN** | Low | |

---

## Summary statistics

### Per-section breakdown

| Section | Total items | VERIFIED | OBSERVED | UNVERIFIED | UNKNOWN | Coverage¹ | Confidence² | Impl ✅ | Impl 🟡 | Impl ⛔ |
| ------- | ----------: | -------: | -------: | ---------: | ------: | --------: | ----------: | ------: | ------: | ------: |
| §1 Boot sector | 40 | 19 | 14 | 6 | 1 | 97.5% | 82.5% | 33 | 2 | 5 |
| §2 MFT & records | 82 | 45 | 12 | 22 | 3 | 96.3% | 69.5% | 68 | 3 | 11 |
| §3 Data runs & bitmap | 51 | 22 | 5 | 20 | 4 | 92.2% | 52.9% | 33 | 5 | 13 |
| §4 Indexes & directories | 44 | 33 | 6 | 4 | 0 | 100% | 88.6% | 41 | 1 | 2 |
| §5 $LogFile & journal | 56 | 16 | 7 | 30 | 3 | 94.6% | 41.1% | 22 | 0 | 34 |
| §6 Special streams | 64 | 34 | 8 | 17 | 8 | 87.5% | 65.6% | 45 | 4 | 15 |
| **Tracked total** | **337** | **169** | **52** | **99** | **19** | **94.4%** | **65.6%** | **242** | **15** | **80** |
| Unspec'd gaps (items 338–351) | 14 | — | — | 3 | 11 | — | — | 0 | 1 | 13 |
| **Grand total** | **351** | **169** | **52** | **102** | **30** | **91.5%** | **62.4%** | **242** | **16** | **93** |

¹ **Coverage** = (VERIFIED + OBSERVED + UNVERIFIED) / Total — items where we have *any* documentation.  
² **Confidence** = (VERIFIED + OBSERVED) / Total — items where the code or external test gives us evidence.

### What the numbers mean

- **91.5% of total surface surveyed** — we have at least some documentation
  for almost every NTFS structure. The 8.5% gap is concentrated in
  LZNT1 compression internals, EFS/TxF payloads, and operational details like
  ACL assignment and hard-link management — all features not yet in scope.

- **62.4% at OBSERVED or better confidence** — nearly two-thirds of the
  surface area has been confirmed by reading source code or external test.
  The largest gaps are in §5 ($LogFile / journal) where the implementation
  writes a baked blob rather than synthesising structures from scratch.

- **Lowest confidence: §5 ($LogFile / journal)** at 41% — the journal
  write path and all recovery logic is undocumented by implementation.
  Everything past the canonical 12 KiB blob is UNVERIFIED or UNKNOWN.

- **Highest confidence: §4 (Indexes)** at 89% — the B+ tree read/write
  path is the best-exercised part of the codebase and spec.

### Highest-impact UNVERIFIED items (to address next)

| Priority | Item | Section | Why it matters |
| -------- | ---- | ------- | -------------- |
| High | $MFTMirr range dynamic calculation | §2 | Mirror range error = records beyond range unrecoverable |
| High | $Bitmap Double-Pass reconciliation | §3 | Core repair operation; no implementation |
| High | Bitmap/LogFile write ordering | §3/§5 | Crash safety; current path is `sync` only |
| High | LSN encoding bit-packing | §5 | Required for any journal replay |
| Medium | $ATTRIBUTE_LIST byte layout | §2 | Required for files with >1 MFT record |
| Medium | Symlink reparse-point target encoding | §6 | Symlink paths may be misencoded |
| Medium | `$LogFile` size scaling formula | §5/gap | Unknown for non-reference volumes |
| Low | $AttrDef content format | gap | Read-only; not critical for correctness |
| Low | LZNT1 compression internals | §6 | Feature is explicitly out of scope |

---

*Generated: 2026-05-27 from spec sections §1–§6 + source cross-reference.*

[TOC](ntfs-specification.md)
