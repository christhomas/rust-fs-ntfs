[TOC](../ntfs-specification.md) | [Next: MFT & records →](02-mft-records.md)

# 1. Volume geometry & boot sector

## Overview {#overview}

An NTFS volume is a contiguous run of equal-sized **clusters** layered on top of
fixed-size **sectors** of the underlying block device. The volume's physical
geometry, addressing units, and locations of the system files needed to bring
the volume online are entirely described by the 512-byte **boot sector** at
sector 0, with a byte-identical **backup boot sector** stored at the last sector
of the partition [UNVERIFIED].

At mount time, every consumer of an NTFS volume — driver, fsck, repair tool,
read-only library — must perform the same first steps:

1. Read sector 0.
2. Validate the boot signature (`0x55AA`), the OEM ID (`"NTFS    "`), the
   logical sector size (must be exactly 512), and the cluster geometry
   (`sectors_per_cluster` is a power of two) [UNVERIFIED].
3. Decode `clusters_per_mft_record` (offset `0x40`) using the signed-byte
   `signed-log2` encoding to derive the MFT record size
   `[OBSERVED: src/mft_io.rs::parse_boot_params_from_bytes]`.
4. Compute the byte offset of `$MFT` from `mft_lcn × cluster_size`.
5. Optionally cross-check against the backup boot sector at
   `partition_start + (TotalSectors − 1) × bytes_per_sector` and reconcile any
   divergence [UNVERIFIED].

Once geometry is parsed, every higher-level structure — MFT records, index
blocks, data runs — addresses storage in **clusters**, never sectors. The
sector size is used solely as the unit of the Update Sequence Array (USA)
fixup applied to multi-sector records (see [§2 USA fixup](02-mft-records.md#usa-fixup)).

`rust-fs-ntfs` implements this parse path in `src/mft_io.rs::read_boot_params`
and uses the resulting `BootParams { bytes_per_sector, sectors_per_cluster,
cluster_size, mft_lcn, file_record_size }` everywhere that needs cluster
arithmetic `[OBSERVED: src/mft_io.rs lines 42–114]`. mkfs builds the inverse
in `src/mkfs.rs::build_boot_sector`.

### Status

| Capability                                 | State |
| ------------------------------------------ | ----- |
| Boot sector parse (read)                   | ✅    |
| Boot sector emit (mkfs)                    | ✅    |
| Backup boot sector emit at last sector     | ✅    |
| Backup boot bidirectional sync / repair    | ⛔    |
| `signed-log2` decode for `0x40` and `0x44` | ✅    |
| BPB checksum validation                    | ⛔    |
| NTFS version detection from `$Volume`      | 🟡    |

## Boot sector layout {#boot-sector-layout}

The NTFS boot sector is 512 bytes, little-endian, and ends in the BIOS
boot signature `0x55 0xAA` at offset `0x1FE`
`[OBSERVED: src/mkfs.rs::build_boot_sector]`.

The first ~36 bytes follow the historical **BIOS Parameter Block (BPB)**
layout inherited from FAT, with most legacy fields zeroed; the NTFS-specific
**extended BPB** starts at offset `0x24` and runs through `0x53`. The
remainder is x86 bootstrap code (offsets `0x54..0x1FE`) terminated by the
boot signature [UNVERIFIED].

### Field table

All offsets are zero-based bytes from the start of sector 0. Sizes are in
bytes. All multi-byte integers are little-endian.

| Offset | Size | Field                              | Notes                                                                                       |
| -----: | ---: | ---------------------------------- | ------------------------------------------------------------------------------------------- |
| `0x000` |   3 | `JmpInstruction`                   | x86 jump to bootstrap. NTFS canonical: `EB 52 90` (jmp +0x52, nop) [UNVERIFIED]              |
| `0x003` |   8 | `OemId`                            | ASCII `"NTFS    "` (4 chars + 4 spaces) [UNVERIFIED]                                         |
| `0x00B` |   2 | `BytesPerSector`                   | Logical sector size. MUST be `512` for NTFS [UNVERIFIED]                                    |
| `0x00D` |   1 | `SectorsPerCluster`                | See [signed-log2 encoding](#log2-encoding); positive values must be a power of two [UNVERIFIED] |
| `0x00E` |   2 | `ReservedSectors`                  | NTFS: `0` [UNVERIFIED]                                                                       |
| `0x010` |   3 | (zero — historical FAT `Fats` byte + 2-byte reserved) | NTFS: `00 00 00` `[OBSERVED: src/mkfs.rs lines 967–970]`                |
| `0x013` |   2 | (zero — historical `RootEntries`)  | NTFS: `00 00` `[OBSERVED: src/mkfs.rs line 969]`                                            |
| `0x015` |   1 | `MediaDescriptor`                  | `0xF8` for fixed disk `[OBSERVED: src/mkfs.rs line 971]`                                     |
| `0x016` |   2 | (zero — historical `SectorsPerFat`) | NTFS: `00 00` `[OBSERVED: src/mkfs.rs line 972]`                                           |
| `0x018` |   2 | `SectorsPerTrack`                  | Cosmetic on modern hardware; conventional `63` `[OBSERVED: src/mkfs.rs line 973]` `[UNVERIFIED]` |
| `0x01A` |   2 | `NumberOfHeads`                    | Cosmetic on modern hardware; conventional `255` `[OBSERVED: src/mkfs.rs line 974]` `[UNVERIFIED]` |
| `0x01C` |   4 | `HiddenSectors`                    | Sector offset of the partition on the containing disk; `0` for unpartitioned images `[OBSERVED: src/mkfs.rs lines 975]` `[UNVERIFIED]` |
| `0x020` |   4 | (zero — historical `LargeSectors`) | NTFS uses the 64-bit field at `0x28` instead `[OBSERVED: src/mkfs.rs line 976]` `[UNVERIFIED]` |
| `0x024` |   4 | `BiosDriveNumberAndExtBpbSig`      | NTFS canonical 4-byte block at `0x24..0x28` is `0x00800080` `[OBSERVED: src/mkfs.rs line 977]` `[UNVERIFIED]` |
| `0x028` |   8 | `TotalSectors64` (NumberSectors)   | Count of *data* sectors in the volume. Microsoft format.com convention: `volume_sectors − 1` (the trailing backup-boot sector is excluded) `[OBSERVED: src/mkfs.rs lines 979–991, docs/mkfs-bug-catalog.md "Bug 6"]` |
| `0x030` |   8 | `MFT_LCN`                          | Starting Logical Cluster Number of `$MFT` [UNVERIFIED]                                       |
| `0x038` |   8 | `MFTMirr_LCN`                      | Starting Logical Cluster Number of `$MFTMirr` [UNVERIFIED]                                   |
| `0x040` |   1 | `ClustersPerFileRecordSegment`     | `signed int8`. See [signed-log2 encoding](#log2-encoding). `0xF6 = −10 → 2^10 = 1024 B`; `0xF4 = −12 → 2^12 = 4096 B` [UNVERIFIED] |
| `0x041` |   3 | (reserved, zero)                   | NTFS: `00 00 00` `[OBSERVED: src/mkfs.rs lines 1008–1010]`                                  |
| `0x044` |   1 | `ClustersPerIndexBuffer`           | Same `signed-log2` encoding as `0x40`, applied to `$INDEX_ALLOCATION` (INDX) page size [UNVERIFIED] |
| `0x045` |   3 | (reserved, zero)                   | NTFS: `00 00 00` `[OBSERVED: src/mkfs.rs lines 1021–1023]`                                  |
| `0x048` |   8 | `VolumeSerialNumber`               | 64-bit unique-per-volume identifier `[OBSERVED: src/mkfs.rs line 1025]`                      |
| `0x050` |   4 | `Checksum`                         | Historically a sum of bytes `0x00..0x50`. Not validated by the major drivers `[OBSERVED: src/mkfs.rs line 1026]` `[UNVERIFIED]` |
| `0x054` |  426 | `BootCode`                         | x86 bootstrap. NTFS does not execute it from a mounted volume; it exists for BIOS-boot scenarios. `rust-fs-ntfs` writes a minimal `CLI; JMP $-1` halt loop `[OBSERVED: src/mkfs.rs lines 1028–1035]` |
| `0x1FE` |   2 | `BootSignature`                    | `55 AA` (little-endian `0xAA55`) `[OBSERVED: src/mkfs.rs lines 1036–1037]` |

### Validation rules (mount-time)

A loader/parser must perform at minimum these checks before trusting any
boot-sector field [UNVERIFIED]:

1. `sector[0x1FE..0x200] == [0x55, 0xAA]`
2. `sector[0x03..0x0B] == "NTFS    "`
3. `BytesPerSector == 512`
4. `SectorsPerCluster != 0` AND `SectorsPerCluster & (SectorsPerCluster − 1) == 0`
   (power of two, when interpreted as an unsigned literal — see
   [signed-log2 encoding](#log2-encoding) for the rare large-cluster case)
5. `TotalSectors64 > 0` and within physical device bounds
6. `MFT_LCN` and `MFTMirr_LCN` are both within volume bounds and non-zero

Failure of any of these rules invalidates the sector. The mount sequence
(see [§1.4 Boot mirror sync](#boot-mirror-sync)) then falls back to the
backup boot sector [UNVERIFIED].

`rust-fs-ntfs` enforces (3), (4) (in unsigned form), and (1) on the
read path; `Ntfs::new` from the upstream parser performs (2) and the
remaining bounds checks before we observe the geometry
`[OBSERVED: src/mft_io.rs lines 68–106; src/facade.rs::Filesystem::mount]`.

## Signed-log2 encoding for cluster-relative sizes {#log2-encoding}

The boot-sector fields at offsets `0x40` (`ClustersPerFileRecordSegment`) and
`0x44` (`ClustersPerIndexBuffer`) use a signed-byte encoding so that record
sizes **smaller than one cluster** can still be expressed [UNVERIFIED].

```
function parse_record_size(boot, offset, cluster_size):
    raw = (int8_t) boot[offset]
    if raw > 0:
        return raw * cluster_size       // record spans `raw` whole clusters
    else:
        return 1 << abs(raw)            // record is 2^|raw| bytes, sub-cluster
```

[UNVERIFIED]

### Worked examples

| Raw byte | Signed value | Decoded size                  |
| -------: | -----------: | ----------------------------- |
| `0x01`   | `+1`         | `1 × cluster_size`            |
| `0x02`   | `+2`         | `2 × cluster_size`            |
| `0x08`   | `+8`         | `8 × cluster_size`            |
| `0xF6`   | `−10`        | `2^10 = 1024` bytes           |
| `0xF4`   | `−12`        | `2^12 = 4096` bytes           |

[UNVERIFIED]

### Why both regimes coexist

When the cluster size is small (e.g., 512 B or 1 KiB), an MFT record is
typically larger than one cluster, so the field expresses a count of
clusters per record (positive value). When the cluster size is large
(≥ 4 KiB), an MFT record is typically smaller than one cluster (1 KiB or
4 KiB), and the field switches to the `2^|n|`-bytes representation
(negative value) [UNVERIFIED].

The same encoding governs `ClustersPerIndexBuffer` at offset `0x44`. With
512-byte clusters, an INDX page size of 4096 bytes spans
`4096 / 512 = 8` clusters and is encoded as `+8`; with 4 KiB clusters, the
same 4096-byte INDX page spans one cluster and is encoded as `+1` [UNVERIFIED].

### Implementation notes

- All bounds checks, USA array sizes, and any per-record buffer allocation
  MUST use the dynamically parsed size. Hardcoding `1024` is fatal on
  4096-byte-record volumes [UNVERIFIED].
- The `sectors_per_cluster` byte at `0x0D` uses a similar but distinct
  encoding: positive (`< 0x80`) means a literal sector count; values `≥ 0x80`
  are interpreted as `1 << (256 − raw)`, used for very large clusters
  (≥ 64 KiB) `[OBSERVED: src/mft_io.rs lines 76–88]` `[UNVERIFIED]`.
- `rust-fs-ntfs::read_boot_params` rejects record sizes outside
  `512 ≤ s ≤ 16384` bytes as implausible
  `[OBSERVED: src/mft_io.rs lines 101–105]`.
- USA fixup-array entry count is `record_size / 512`, derived from the
  decoded record size [UNVERIFIED].

## $Boot file (record 7) and backup boot sector {#boot-mirror-sync}

The boot sector is also exposed as a regular NTFS file under MFT record 7
(`$Boot`). Its `$DATA` attribute is non-resident and, by convention, claims
the first 8 KiB of the volume (cluster 0 onwards) `[OBSERVED: src/mkfs.rs
lines 691–725, 384–391]`.

#### `mft_lcn` placement must not overlap `$Boot.$DATA` {#mft-lcn-placement}

Because `$Boot.$DATA` claims the first 8 KiB of the volume starting at
LCN 0, the MFT's first LCN must lie *outside* that range. The constraint
is:

```
mft_lcn ≥ ceil(8192 / cluster_size)
```

At 4 KiB clusters the canonical `mft_lcn = 4` satisfies this (`$Boot.$DATA`
occupies LCN 0..1, the MFT starts at LCN 4). At smaller cluster sizes
(1 KiB or 512 B) a hardcoded `mft_lcn = 4` puts the MFT *inside*
`$Boot.$DATA`'s mapping and `chkdsk` aborts with "Corrupt master file
table. CHKDSK aborted." `rust-fs-ntfs::mkfs` computes
`mft_lcn = max(4, ceil(8192 / cluster_size))`
[`[OBSERVED: src/mkfs.rs / mft_lcn placement]`](#references), which gives:

| cluster_size | mft_lcn |
| ------------ |  ------ |
| 512 B        | 16      |
| 1 KiB        | 8       |
| 4 KiB        | 4       |
| ≥ 8 KiB      | 4       |

#### Bootstrap code area {#bootstrap-code}

The first 426 bytes of the boot sector (offsets `0x54..0x1FE`) hold
the BIOS bootstrap code that runs on a BIOS-boot path to load `ntldr` /
`bootmgr`. On non-BIOS volumes (data partitions, snapshots, mounted
images), `ntfs.sys` never executes this region; `chkdsk` does not
validate its contents.

`rust-fs-ntfs::mkfs` ships a 3-byte clean-room halt loop (`FA EB FD` —
`CLI; JMP $-1`) in this region rather than baking Microsoft's compiled
bootstrap bytes verbatim, keeping the repo free of Microsoft binary
code while still producing a valid (mountable, write-accepting, chkdsk-
clean) NTFS volume. Matrix scenarios confirm this is sufficient: 42/42
sealed runs pass with the 3-byte halt loop and no BIOS-boot scenarios
in scope.

### Two physical copies

| Copy             | Location                                                       |
| ---------------- | -------------------------------------------------------------- |
| Primary boot     | Sector 0 (byte offset 0)                                       |
| Backup boot      | Last 512-byte sector of the partition: `partition_start + (TotalSectors − 1) × bytes_per_sector` [UNVERIFIED] |

Equivalently, with `volume_bytes = TotalClusters × cluster_size`, the backup
lives at byte offset `volume_bytes − bytes_per_sector`
`[OBSERVED: src/mkfs.rs lines 319–330; docs/mkfs-bug-catalog.md "Bug 7"]`.

`rust-fs-ntfs::mkfs` writes the backup at exactly this last 512-byte sector.
An earlier iteration wrote it at the start of the *last cluster* instead;
this caused the kernel mount path to find no signature at the location it
probes via `BPB.NumberSectors`, producing Event 55 at small volumes
`[OBSERVED: docs/mkfs-bug-catalog.md "Bug 7"]`.

A complementary observation: writing the backup at *both* positions
(start-of-last-cluster AND last-sector) caused Event 55 at ≥ 256 MiB when
two valid signatures coexisted near the volume tail; the last-sector-only
placement is the canonical layout `[OBSERVED: docs/mkfs-bug-catalog.md "Bug 7"]`.

### Bidirectional synchronization (Phase 0)

When mounting, the canonical recovery flow reads both copies and reconciles
them [UNVERIFIED]:

```
function boot_sector_sync(io):
    primary = io.read(offset=0, len=512)
    backup  = io.read(offset=io.total_bytes - 512, len=512)

    p_valid = validate_boot(primary)
    b_valid = validate_boot(backup)

    if p_valid AND b_valid:        return primary
    if p_valid AND NOT b_valid:    write(backup_offset, primary); return primary
    if NOT p_valid AND b_valid:    write(0, backup); return backup
    if NOT p_valid AND NOT b_valid: FATAL; abort()
```

[UNVERIFIED]

Specific contracts:

- **Primary corrupt, backup valid** → overwrite primary with backup, but
  reconcile critical geometry fields (`TotalSectors` may be smaller on the
  backup if the partition was resized after format) rather than performing a
  blind byte-for-byte copy [UNVERIFIED].
- **Primary valid, backup corrupt** → overwrite backup with primary [UNVERIFIED].
- **Field-level correction** is permitted: if the sector is largely intact
  but specific BPB fields are invalid, individual fields can be repaired in
  place [UNVERIFIED].
- **Both corrupt** → fatal; the volume is unmountable [UNVERIFIED].

`rust-fs-ntfs` does not yet implement the repair side of this protocol; it
relies on the primary copy being intact `[OBSERVED: src/facade.rs::mount —
no backup-boot fallback path]`.

### `$Boot` MFT record vs. raw sector

`$Boot` (record 7) is a regular non-resident file whose first cluster
overlays the boot sector itself. Its `$DATA` attribute typically declares an
8 KiB value spanning the first `ceil(8192 / cluster_size)` clusters; on a
512-byte cluster volume that is 16 clusters, on a 4 KiB cluster volume it
is 2 clusters `[OBSERVED: src/mkfs.rs lines 691–725, 386–391]`.

Both the bitmap allocation and the `$DATA` cluster runs must be consistent
with this — historic versions of `rust-fs-ntfs::mkfs` allocated only one
cluster, which on small-cluster volumes caused chkdsk to report
`Found 0x1 clusters allocated to file "$Boot"` errors
`[OBSERVED: src/mkfs.rs lines 384–391]`.

## Sector vs cluster {#sector-vs-cluster}

| Concept | Definition                                    | Encoded in boot                       |
| ------- | --------------------------------------------- | ------------------------------------- |
| Sector  | The underlying device's logical block.        | `BytesPerSector` at `0x0B`            |
| Cluster | NTFS's allocation unit. Always ≥ one sector.  | `SectorsPerCluster` at `0x0D`         |

### Hard rules

- **Logical sector size MUST be 512 bytes** in the BPB. Windows refuses to
  mount NTFS volumes with a logical sector size of 1024, 2048, or 4096 in
  the BPB `[UNVERIFIED]` (cross-reference vs `[MS-NTFS]` §2.2
  pending).
- `SectorsPerCluster` MUST be a power of two (or a valid signed-byte
  large-cluster shift; see [signed-log2](#log2-encoding)) [UNVERIFIED].
- Cluster size = `BytesPerSector × SectorsPerCluster`
  `[OBSERVED: src/mft_io.rs line 89]`.

### 512e and 4Kn drives

A 4K Native (4Kn) device usually emulates a 512-byte logical sector at the
filesystem level (512e). The NTFS boot sector still records
`BytesPerSector = 512`, while the underlying hardware physical block is
`4096`. Repair / I/O code must align buffers to the *physical* sector size
to avoid Read-Modify-Write penalties or silent misalignment, even though
addressing arithmetic uses the logical 512 [UNVERIFIED].

### Allowed cluster sizes

Supported cluster sizes range from 512 B up to 2 MiB,
validated dynamically from `sectors_per_cluster` in the boot sector
[UNVERIFIED].

Common Windows defaults (modern):

| Volume size           | Default cluster size                                  |
| --------------------- | ----------------------------------------------------- |
| ≤ 16 TB               | 4 KiB `[UNVERIFIED]` (cross-reference vs MSDN pending) |
| > 16 TB               | larger, scaled to keep `LCN` in 32-bit range historically `[UNVERIFIED]` |

> **Rule (implementation):** Any cluster-size assumption (e.g., hardcoded
> 4 KiB) is a fatal flaw. Modern Windows Server volumes routinely use 8 KiB,
> 16 KiB, 64 KiB, or even 2 MiB clusters; cluster-size arithmetic MUST be
> derived dynamically from the boot sector [UNVERIFIED].

## Cluster cache / I/O subsystem note {#io-subsystem}

The boot sector defines the addressing units used by every other on-disk
structure. For runtime I/O, NTFS readers maintain a runlist cache that
translates Virtual Cluster Numbers (VCN) inside a file to Logical Cluster
Numbers (LCN) on disk, using entries of the form
`{ start_vcn: u64, lcn: u64, count: u64 }` (24 bytes each) [UNVERIFIED].

Two invariants belong here even though the cache itself
is described in [§3 Data runs](03-data-runs-bitmap.md):

1. **All VCN/LCN fields MUST be `u64`.** A `u32` overflows at 2³² clusters,
   which at 4 KiB clusters caps the volume at 16 TiB — well below NTFS's
   256 TiB limit [UNVERIFIED].
2. **VCN→LCN translation is binary search over the cache**, not re-parsing
   the data run on every read [UNVERIFIED].

`rust-fs-ntfs` implements the runlist parse in `src/data_runs.rs` and the
cluster-level read/write primitives in `src/block_io.rs`; the boot-sector
parse is the single point of entry that primes both
`[OBSERVED: src/lib.rs module list lines 37–52]`.

## NTFS version detection {#ntfs-versions}

The boot sector does **not** itself encode the NTFS version. After the boot
sector is validated, the version is read from `MFT record 3` (`$Volume`)
in the `$VOLUME_INFORMATION` attribute (type `0x70` for NTFS attribute IDs)
[UNVERIFIED].

The version determines which validation features apply.

| Version | OS origin     | `MajorVersion` | `MinorVersion` | CRC32 in MFT records | `$UsnJrnl`         | 8-byte-aligned attrs | Notes                                 |
| ------- | ------------- | -------------: | -------------: | :------------------: | :----------------: | :------------------: | ------------------------------------- |
| 1.2     | NT 4.0 / 9x   | 1              | 2              | ❌                   | ❌                 | ❌                   | Skip CRC32 step entirely [UNVERIFIED] |
| 3.0     | Windows 2000  | 3              | 0              | ❌                   | ⚠️ optional        | ✅                   | `$UsnJrnl` may be present but disabled [UNVERIFIED] |
| 3.1     | XP / Vista+   | 3              | 1              | ✅                   | ✅                 | ✅                   | Mandatory CRC32 in MFT record headers [UNVERIFIED] |

Source: [UNVERIFIED].

### Detection rules

- If `MajorVersion == 1`: the engine MUST skip the CRC32 checksum step on
  MFT records. Applying CRC32 to a 1.2 record where the last 4 bytes are
  ordinary data would corrupt healthy records [UNVERIFIED].
- If `$VOLUME_INFORMATION` is unreadable: default to the **most permissive**
  mode (assume 1.2, no CRC32) and log a warning. Defaulting strict would
  invalidate every record on a legacy volume [UNVERIFIED].
- All version-conditional behavior MUST be gated on a single state-object
  flag set once during the discovery phase, not re-tested per record
  [UNVERIFIED].

### `$UpCase` collation

Independent of version, the engine MUST use the volume's own `$UpCase`
table (MFT record 10) for case-insensitive collation in `$I30` B-trees and
namespace validation. Using the host OS's Unicode tables (glibc, ICU)
risks rendering files invisible to Windows because the volume may have been
formatted with a different Unicode revision [UNVERIFIED].

If `$UpCase` is missing or destroyed, fall back to a statically compiled
NT-era Windows uppercase mapping and log `WARN_UPCASE_FALLBACK`. Uncommon
Unicode filenames may sort incorrectly in this mode [UNVERIFIED].

`rust-fs-ntfs` carries a canonical `upcase-canonical.bin` and emits it
during mkfs; runtime collation is provided by `src/upcase.rs`
`[OBSERVED: src/lib.rs module list line 51; src/upcase-canonical.bin]`.

### Implementation status

`rust-fs-ntfs::Filesystem::volume_info` currently returns a hardcoded
`(major=3, minor=1)` rather than reading `$VOLUME_INFORMATION`. This is
correct for any volume `mkfs` produces (we only emit 3.1) but is not a
true detection — reading legacy volumes will mislabel them
`[OBSERVED: src/facade.rs lines 165–179]`.

## Volume geometry limits {#geometry-limits}

| Parameter                | Range                                  | Source           |
| ------------------------ | -------------------------------------- | ---------------- |
| Cluster size             | 512 B – 2 MiB                          | [UNVERIFIED]     |
| MFT record size          | 1024 B, 4096 B (the only sizes seen)   | [UNVERIFIED]     |
| INDX page size           | 4096 B (standard)                      | [UNVERIFIED]     |
| Volume size              | up to 256 TiB                          | [UNVERIFIED]     |
| Max file size            | up to 16 TiB (with standard clusters)  | [UNVERIFIED]     |
| Sector size (logical)    | 512 B (hard requirement)               | [UNVERIFIED]     |
| Sector size (physical)   | 512 B or 4096 B (4Kn)                  | [UNVERIFIED]     |

The 256 TiB volume cap derives from the 64-bit LCN representation crossed
with the maximum cluster size [UNVERIFIED]. With 4 KiB clusters
and a 32-bit LCN, the cap would have been 16 TiB; using `u64` everywhere
preserves the full 256 TiB headroom [UNVERIFIED].

### MFT growth limits

The MFT itself is a regular NTFS file (record 0, `$MFT`) with a non-resident
`$DATA` attribute. Its growth is bounded by:

- The free-cluster supply tracked by `$Bitmap` (record 6).
- The data-run encoding limits described in [§3 Data runs](03-data-runs-bitmap.md).
- Per-attribute allocated-size accounting (`AllocatedSize` field on the
  `$DATA` attribute header).

There is no boot-sector-level cap on MFT size beyond what the volume
geometry implies `[UNVERIFIED]`.

## Magic numbers and signatures {#signatures}

The following magic byte sequences appear across the on-disk format. They
are gathered here as a single reference; per-structure usage is documented
in the section listed.

| Magic            | Bytes (LE)                         | Where                                   | Section                                         | Source                                       |
| ---------------- | ---------------------------------- | --------------------------------------- | ----------------------------------------------- | -------------------------------------------- |
| `OemId`          | `4E 54 46 53 20 20 20 20` (`"NTFS    "`) | Boot sector `0x03..0x0B`           | §1 (this section)                               | [UNVERIFIED]                                 |
| `BootSignature`  | `55 AA` (= `0xAA55` LE16)          | Boot sector `0x1FE`                     | §1 (this section)                               | [UNVERIFIED]                                 |
| `FILE`           | `46 49 4C 45`                      | First 4 bytes of every MFT record       | [§2 MFT records](02-mft-records.md#mft-record-header) | `[OBSERVED: src/mft_io.rs line 122]` |
| `INDX`           | `49 4E 44 58`                      | First 4 bytes of every `$INDEX_ALLOCATION` block | [§4 Indexes](04-indexes-directories.md)  | `[OBSERVED: src/idx_block.rs line 151]` |
| `RSTR`           | `52 53 54 52`                      | First 4 bytes of `$LogFile` restart pages (pages 0, 1) | [§5 LogFile](05-logfile-journal.md) | `[OBSERVED: src/mkfs.rs lines 135–138]` |
| `RCRD`           | `52 43 52 44`                      | First 4 bytes of `$LogFile` record pages (pages ≥ 2) | [§5 LogFile](05-logfile-journal.md) | `[OBSERVED: src/mkfs.rs lines 141–148]` |
| `BAAD`           | `42 41 41 44`                      | Replaces `FILE`/`INDX`/`RCRD` magic when the driver detects a torn-write or fixup mismatch and marks the record as bad | [§2 MFT records](02-mft-records.md#usa-fixup) | `[UNVERIFIED]` (cross-reference vs `[MS-NTFS]` and Windows Internals pending) |
| `TXBG`           | `54 58 42 47` (= `0x54584247` LE32) | `BEGIN_TX` log record header           | [§5 LogFile](05-logfile-journal.md)             | [UNVERIFIED]                                 |
| `TXCM`           | `54 58 43 4D` (= `0x5458434D` LE32) | `COMMIT_TX` log record header          | [§5 LogFile](05-logfile-journal.md)             | [UNVERIFIED]                                 |

### USA fixup placeholder

The two-byte words at `0x1FE..0x200` of every 512-byte sector inside any
`FILE` / `INDX` / `RCRD` record are *not* the boot-sector signature even
though they share an offset within the sector — they are the per-sector
USA tail bytes, replaced on read with the originals from the record
header's USA. The boot sector itself is single-sector and is *not* USA-
protected — its `0x1FE` bytes ARE the literal `55 AA` boot signature
`[OBSERVED: src/mft_io.rs lines 30–36]`.

The full USA mechanism is defined once in
[§2 USA fixup](02-mft-records.md#usa-fixup) and referenced from §4 and §5.

## References

- `[OBSERVED: src/mkfs.rs::build_boot_sector]` — boot sector emit path
  (`src/mkfs.rs` lines 944–1039).
- `[OBSERVED: src/mft_io.rs::parse_boot_params_from_bytes]` — boot sector
  parse path (`src/mft_io.rs` lines 42–114).
- `[OBSERVED: src/facade.rs::Filesystem]` — mount entry point and
  `volume_info` (`src/facade.rs` lines 102–179).
- `[OBSERVED: docs/mkfs-bug-catalog.md "Bug 6"]` — `BPB.NumberSectors = N − 1`
  convention.
- `[OBSERVED: docs/mkfs-bug-catalog.md "Bug 7"]` — backup boot at last
  512-byte sector (not start of last cluster).
- `[OBSERVED: docs/STATUS.md]` — `read_boot_params` test coverage
  description.

## Open questions

`[UNVERIFIED]` claims local to this section. Cross-references against
`[MS-NTFS]`, `[MS-FSCC]`, MSDN, and `ntfs.com` are pending.

- [ ] (2026-05-03) `SectorsPerTrack = 63`, `NumberOfHeads = 255` are
  cosmetic on modern hardware — no permitted source states whether any
  consumer validates them. Source: `src/mkfs.rs` conventions only.
  Test: format with non-default values, verify Windows mount behavior.
- [ ] (2026-05-03) `HiddenSectors` semantics for unpartitioned vs
  partitioned images — no permitted source. Test: dump partition images
  vs raw disk images and compare.
- [ ] (2026-05-03) The `0x24..0x28` 4-byte block (`0x00800080` in our
  emitter) — Windows-Internals-derived "signature byte for NTFS"; not
  yet corroborated by any permitted source. Test: cross-reference vs
  `[MS-NTFS]`.
- [ ] (2026-05-03) Boot-sector `Checksum` field at `0x50..0x54` —
  historically a sum of bytes `0x00..0x50`. No permitted source states
  whether it is validated; `rust-fs-ntfs` writes zero and ntfs.sys /
  chkdsk accept it. Test: flip a byte and observe whether any consumer
  rejects.
- [ ] (2026-05-03) `SectorsPerCluster` large-value encoding (`raw ≥ 0x80`
  ⇒ `1 << (256 − raw)`) for cluster sizes ≥ 64 KiB — `rust-fs-ntfs`
  parses this; the encoding has not been corroborated against a
  permitted source. Test: format with 64 KiB / 128 KiB / 1 MiB / 2 MiB
  clusters and verify on-disk byte at `0x0D`.
- [ ] (2026-05-03) Windows refuses to mount with logical sector size
  `≠ 512` — asserted in lead material; not yet experimentally confirmed
  by `rust-fs-ntfs`. Test: emit a boot with `BytesPerSector = 4096` and
  observe Windows mount behavior.
- [ ] (2026-05-03) Default cluster sizes Microsoft format.com selects per
  volume size — claimed in this section as "4 KiB up to 16 TiB" but no
  permitted source cites it. Test: format reference volumes at multiple
  sizes and dump `0x0D`.
- [ ] (2026-05-03) `BAAD` magic — used by Windows to mark torn /
  fixup-failed records. Not yet corroborated by any permitted source.
  Test: cross-reference vs `[MS-NTFS]` / `[MSDN]`; dump a deliberately
  torn record after a ntfs.sys / chkdsk pass.
- [ ] (2026-05-03) Global MFT size limit — no permitted source states a
  global cap. Test: cross-reference vs `[MS-NTFS]` for any documented
  MFT size cap.

[TOC](../ntfs-specification.md) | [Next: MFT & records →](02-mft-records.md)
