# mkfs_ntfs bug catalog — discoveries, fixes, evidence, references

> **PROCESSED** into [`chkdsk-improvement-findings.md`](./chkdsk-improvement-findings.md) on 2026-05-02. Content below preserved verbatim within an HTML comment for audit; nothing deleted.

<!-- BEGIN-PROCESSED-INTO-chkdsk-improvement-findings

A consolidated reference for everything the multi-agent test matrix
has surfaced about `mkfs_ntfs`. Per-bug entries cover:

- **Symptom** — verbatim chkdsk / Event Log / `Get-Volume` output.
- **Diagnostic** — what we ran, what we observed.
- **Root cause** — what was wrong in our code, expressed against the
  publicly documented NTFS layout.
- **Fix** — minimal change description + commit hash.
- **Justification** — why this change is correct. Cites the public
  spec (Microsoft MS-FSCC, Windows Internals, byte-diff against
  Microsoft `format.com /FS:NTFS` reference) — never any GPL'd
  reverse-engineered Linux NTFS source.
- **Verification** — what observable behaviour changed after the fix.
- **Status in work-list** — which scenarios moved on which fix.

For full per-iteration history including dead ends, see
[chkdsk-findings.md](./chkdsk-findings.md). For methodology see
[multi-agent-test-protocol.md](./multi-agent-test-protocol.md).
For the corroboration mechanism (Mac → VM → Mac local pipeline) see
[local-test-pipeline.md](./local-test-pipeline.md).

## Glossary of recurring proper nouns

- **chkdsk DRIVE:** — Microsoft's read-only filesystem checker. Stages
  1 and 2 walk the MFT and the index trees respectively. Exit codes:
  0 = clean; 1 = errors fixed; 2 = restart required; 3 = could not
  check / errors.
- **`format.com /FS:NTFS`** — Microsoft's NTFS formatter. The
  canonical reference our writer is benchmarked against. Lives in
  every Windows install; the local pipeline runs it on a parallel
  VHDX of the same size.
- **`frs.cxx 60f`** — internal chkdsk error string. The trailing
  "An unspecified error occurred (6672732e637878 60f)" decodes to
  ASCII `frs.cxx` + offset `0x60f` — a chkdsk-internal assertion
  in their MFT-record validation code (`frs.cxx`). Currently the
  ceiling all otherwise-clean scenarios bottom out at; tracked for
  future iteration (see "Outstanding" section below).
- **MS-FSCC** — Microsoft's published "[MS-FSCC] File System
  Algorithms" specification. Authoritative source for attribute
  layouts, $FILE_NAME body, security descriptors, etc.
- **USA** — Update Sequence Array. NTFS stamps the last 16 bits of
  every sector in an MFT record with a USN; the original last-words
  are saved in a separate array near the record header. Read-back
  reverses the substitution. Detects torn writes.

## Bug 1 — `$FILE_NAME` `indexed_flag` zero on every system record (iter6, iter9)

### Symptom

`chkdsk DRIVE:` reports:

```
Stage 1: Examining basic file system structure ...
Attribute record (30, "") from file record segment 0 is corrupt.
Attribute record (30, "") from file record segment 1 is corrupt.
[...repeats for segments 0..0xB...]
Errors found.  CHKDSK cannot continue in read-only mode.
```

### Diagnostic

Per-record byte-diff of `$FILE_NAME` (attribute type 0x30) on system
records 0, 1, 5, 6, 10 against `format.com` reference:

| Rec | namespace ref/ours | indexed_flag ref/ours | alloc/real ref      | alloc/real ours |
|-----|--------------------|-----------------------|---------------------|-----------------|
| 0   | 3 / 3 ✓            | **1 / 0 ✗**           | 0x10000 / 0x10000   | **0 / 0 ✗**     |
| 1   | 3 / 3 ✓            | **1 / 0 ✗**           | 0x4000 / 0x4000     | **0 / 0 ✗**     |
| 5   | 3 / 3 ✓            | **1 / 0 ✗**           | 0 / 0 ✓             | 0 / 0 ✓         |
| 6   | 3 / 3 ✓            | **1 / 0 ✗**           | 0x3000 / 0x2E00     | **0 / 0 ✗**     |
| 10  | 3 / 3 ✓            | **1 / 0 ✗**           | 0x20000 / 0x20000   | **0 / 0 ✗**     |

### Root cause

Two distinct fields in the resident `$FILE_NAME` attribute header /
body were wrong:

1. **Attribute header offset 0x16 (`indexed_flag`).** Spec describes
   this byte as "Resident: indexed flag (1 if attribute referenced
   from index)". Every `$FILE_NAME` is referenced from the parent
   directory's `$I30` index; we wrote 0.
2. **`$FILE_NAME` value bytes 0x28..0x30 (`allocated_size`) and
   0x30..0x38 (`real_size`).** These mirror the underlying `$DATA`'s
   sizes. We wrote 0 even when the record had a non-empty `$DATA`.

### Fix

`src/mkfs.rs::write_file_name`:
- Set `rec[at + 22] = 1` (indexed_flag).
- Add `data_alloc: u64, data_real: u64` parameters; write them at
  value-bytes 0x28..0x30 and 0x30..0x38.

Each system-record call site supplies the correct sizes (e.g. `$MFT`
gets `mft_clusters * cluster_size`).

### Justification

Spec citation: publicly documented NTFS resident-attribute header
layout (`indexed_flag` at +0x16) and `$FILE_NAME` body layout
(allocated/real at +0x28/+0x30). Byte-diff in CI run 25234929879
showed every reference record had `indexed_flag=1` and matching
sizes; ours differed.

### Verification

`Attribute record (30, "") is corrupt` errors gone for all 12 system
records. Surfaced the next-layer error class ("First free byte offset
corrected") which led to Bug 2.

### Iteration

iter9 in [chkdsk-findings.md](./chkdsk-findings.md).

## Bug 2 — MFT record `bytes_used` off by 4 (iter9, iter10)

### Symptom

After Bug 1's fix, `chkdsk` reports:

```
First free byte offset corrected in file record segment 0.
First free byte offset corrected in file record segment 1.
[...repeats for all 12 system records...]
Errors found.  CHKDSK cannot continue in read-only mode.
```

### Per-field diff

| Rec | ref bytes_used | ref end_marker_at | ours bytes_used | ours end_marker_at |
|-----|----------------|-------------------|-----------------|--------------------|
| 0   | 0x210          | 0x208             | 0x17C           | 0x178              |
| 1   | 0x1D0          | 0x1C8             | 0x164           | 0x160              |
| 5   | 0x680          | 0x678             | 0x15C           | 0x158              |
| 11  | 0x130          | 0x128             | 0x164           | 0x160              |

Pattern: reference always sets `bytes_used = end_marker_offset + 8`;
ours always set `bytes_used = end_marker_offset + 4`.

### Root cause

The NTFS attribute end-marker is **8 bytes** total: type=0xFFFFFFFF (4
bytes) + length=0 (4 bytes). Our cursor advanced by 4 after writing
the marker; spec requires 8.

### Fix

`src/mkfs.rs::build_system_record`: advance cursor by 8 after
writing the end marker, not 4.

### Justification

Spec: NTFS attribute records are terminated by an 8-byte sentinel
where both the type code (0xFFFFFFFF) and the length field (0) are
explicit. Microsoft's `format.com` reference writes 8 bytes past the
last real attribute; our writer wrote 4.

### Iteration

iter10 in [chkdsk-findings.md](./chkdsk-findings.md).

## Bug 3 — sequence number always 1 (iter10, iter11)

### Symptom

After Bug 2's fix, records 0 and 1 verify clean but records 2..0xB
report:

```
Incorrect information was detected in file record segment N.
```

### Per-field diff

| Rec | ref seq | ours seq | parent_ref (both) |
|-----|---------|----------|-------------------|
| 0   | 1       | 1        | (rec=5, seq=5)    |
| 1   | 1       | 1        | (rec=5, seq=5)    |
| 2   | 2       | **1**    | (rec=5, seq=5)    |
| 3   | 3       | **1**    | (rec=5, seq=5)    |
| 4   | 4       | **1**    | (rec=5, seq=5)    |
| 5   | 5       | **1**    | (rec=5, seq=5)    |
| ... | ...     | **1**    | (rec=5, seq=5)    |
| 11  | 11      | **1**    | (rec=5, seq=5)    |

### Root cause

System records' children claim parent reference (rec=5, seq=5), but
our root directory at rec 5 has seq=1 — mismatch. Microsoft assigns
each system record `sequence = max(1, rec_number)`, so the root has
seq=5 and the (5,5) parent reference resolves cleanly.

### Fix

`src/mkfs.rs::build_system_record`: `seq = max(1, rec_num)` for
system records. Records 0 and 1 still get seq=1; records 2..11 get
their record number as the sequence.

### Justification

Empirical (the byte-diff is the proof). Microsoft's
`format.com` does the same. The (5, 5) parent_reference cannot
resolve unless rec 5 has seq=5.

### Iteration

iter11 in [chkdsk-findings.md](./chkdsk-findings.md).

## Bug 4 — `$Secure` missing the view-index flag (iter12)

### Symptom

After Bugs 1–3 fixed, chkdsk reports:

```
Flags for file record segment 9 are incorrect.
```

### Diagnostic

Reference's rec 9 is `$Quota` (modern Microsoft format moves `$Secure`
out of the first 16 records). Ours keeps `$Secure` at slot 9 (NTFS-3.0
era convention). Reference rec 9 has flags=0x0001; ours flags=0x0001.
Identical at the byte level — but chkdsk identifies our rec by *name*
(`$Secure`) and demands the view-index bit on its MFT header.

### Per-spec layout

`_FILE_RECORD_SEGMENT_HEADER.Flags` at offset 0x16:

- 0x0001 `MFT_RECORD_IN_USE`
- 0x0002 `MFT_RECORD_IS_DIRECTORY` (record hosts a $FILE_NAME-keyed
  index — i.e. an ordinary directory)
- 0x0004 reserved
- 0x0008 `MFT_RECORD_IS_VIEW_INDEX` (record hosts a *named view
  index* — anything indexing something other than $FILE_NAME, e.g.
  `$Secure`'s `$SDH`/`$SII`, `$Quota`'s `$O`/`$Q`, `$ObjId`'s `$O`,
  `$Reparse`'s `$R`)

`$Secure` is the canonical view-index host. chkdsk has hardcoded
knowledge of `$Secure` and demands the IS_VIEW_INDEX bit even when
the on-disk view-index attributes are absent.

### Root cause

Pure flag-bits omission on rec 9.

### Fix

`src/mkfs.rs::build_system_record`:

```rust
let is_view_index = record_number == rec::SECURE;
let flags: u16 = 0x0001
    | if is_dir { 0x0002 } else { 0x0000 }
    | if is_view_index { 0x0008 } else { 0x0000 };
```

### Justification

MS-FSCC `_FILE_RECORD_SEGMENT_HEADER.Flags` field reference. The
view-index bit is observable empirically in any Microsoft-formatted
volume that ships a `$Secure` record; chkdsk treats the bit as
mandatory.

### Iteration

iter12 in [chkdsk-findings.md](./chkdsk-findings.md).

## Bug 5 — root directory's `$INDEX_ROOT '$I30'` was empty (iter13)

### Symptom

After Bug 4 fix, chkdsk Stage 2:

```
Stage 2: Examining file name linkage ...
Index verification completed.
CHKDSK is scanning unindexed files for reconnect to their original directory.
Detected orphaned file $MFT (0), should be recovered into directory file 5.
Detected orphaned file $MFTMirr (1), should be recovered into directory file 5.
[...all 12 system records 0..11...]
Skipping further messages about recovering orphans.
An unspecified error occurred (6672732e637878 60f).
```

### Per-field diff (rec 5 root `INDEX_ROOT '$I30'`)

| Field                  | reference | ours        |
|------------------------|-----------|-------------|
| INDEX_ROOT attr length | 0x488     | 0x50        |
| value size             | 0x468     | 0x30        |
| INDEX_HEADER.idx_len   | 0x458     | 0x20        |
| INDEX_ENTRY count      | 12 + LAST | LAST only   |

Reference's 12 entries (sorted by COLLATION_FILE_NAME): `$AttrDef`,
`$BadClus`, `$Bitmap`, `$Boot`, `$LogFile`, `$MFT`, `$MFTMirr`,
`$Quota`, `$UpCase`, `$Volume`, `.`, plus LAST sentinel.

### Root cause

NTFS requires every file's parent's `$I30` index to contain an
INDEX_ENTRY referencing the child via `(rec_num, sequence)` and
carrying the child's `$FILE_NAME` stream. The 12 system records all
declare parent = (5, 5), so root must list all of them. We shipped
an `$INDEX_ROOT` with only the LAST sentinel — no entries — because
`build_empty_index_root_attr` literally built that.

### Fix (`src/mkfs.rs`)

- Collect `(rec_num, name, is_dir, data_alloc, data_real)` per
  system record during build.
- Move rec 5 build to **after** rec 11 so all child metadata is
  available.
- Sort the entries by COLLATION_FILE_NAME (UTF-16-LE bytewise after
  ASCII upcase — pure-ASCII names match Microsoft's order).
- Emit one INDEX_ENTRY per record with a `$FILE_NAME` stream
  byte-identical to the in-record `$FILE_NAME` attribute value
  (parent=(5,5), seq=max(1, rec_num) per Bug 3, alloc/real sizes
  per Bug 1).
- Terminate with the LAST sentinel.

Helpers added: `build_file_name_stream`, `build_index_entry`,
`build_populated_index_root_attr`, `collate_file_name`,
`ascii_upcase16`.

### Justification

Spec: any directory's `$I30` is the authoritative "what does this
directory contain" structure. chkdsk's Stage 2 walks the MFT,
finds in-use records claiming root as parent, then verifies they
appear in root's `$I30`. Missing entries = orphan. Microsoft's
`format.com` ships the index populated with all system files;
the byte-diff is the proof.

### Verification

All 12 orphan-recovery lines disappear post-fix. Stage 1 + Stage 2
verify cleanly. The trailing `frs.cxx 60f` is a residual from a
deeper issue, not this bug.

### Iteration

iter13 in [chkdsk-findings.md](./chkdsk-findings.md). Independently
verified on `mac-format-label-cjk` (iter13b) — same fix, byte-perfect
result.

## Bug 6 — BPB `NumberSectors` off by one (iter14)

### Symptom

`mac-format-tiny-32mib`: 32 MiB volume, every Windows operation
fails. Get-Volume reports `FileSystemType=Unknown, Size=0`.

### Per-field diff (NTFS BPB)

| Offset | Field         | reference (96 MiB)     | ours pre-fix (32 MiB) |
|--------|---------------|------------------------|------------------------|
| 0x28   | NumberSectors | 0x2FEFF (= N − 1)      | 0x10000 (= N)          |

### Root cause

BPB.NumberSectors at offset 0x28 is the count of *data* sectors —
**not counting the trailing backup-boot sector**. Microsoft's
convention is `NumberSectors = volume_sectors − 1`. Our writer
wrote N (the full sector count). At ≥ 256 MiB, ntfs.sys tolerated
the off-by-one; at 32 MiB it did not.

### Fix (`src/mkfs.rs::build_boot_sector`)

```rust
let volume_sectors: u64 = cluster_count * cluster_size as u64 / bytes_per_sector as u64;
let number_sectors: u64 = volume_sectors - 1;
b[0x28..0x30].copy_from_slice(&number_sectors.to_le_bytes());
```

### Justification

Empirical against Microsoft `format.com` reference (a 96 MiB volume
showed NumberSectors = 0x2FEFF for 196352 partition sectors, i.e.
N − 1). Aligns with the publicly documented NTFS BPB convention.

### Iteration

iter14 in [chkdsk-findings.md](./chkdsk-findings.md).

## Bug 7 — backup boot sector at start of last cluster, not last sector (iter15)

### Symptom

After Bug 6, 32 MiB still refuses to mount. NTFS Event ID 55:

> A corruption was discovered in the file system structure on volume X.
> The exact nature of the corruption is unknown.  The file system
> structures need to be scanned and fixed offline.

### Per-byte diff (`mac-format-tiny-32mib`, 32 MiB / 4096 cluster)

| byte offset (in volume)         | content     | who reads it       |
|---------------------------------|-------------|--------------------|
| start-of-last-cluster (33550336) | boot copy   | (no consumer at small volumes) |
| last-sector (33553920)          | zeros       | **ntfs.sys at small volumes — finds no signature → Event 55** |

### Root cause

Pure layout error in the last write of the boot sector. mkfs wrote
the backup at `(cluster_count - 1) * cluster_size` = byte
`volume_size - cluster_size` = start of the last *cluster* (sector
65528). ntfs.sys reads BPB.NumberSectors and probes byte
`number_sectors * bytes_per_sector` = `volume_size - bytes_per_sector`
= last 512-byte *sector* (sector 65535). Off by 7 sectors / 3584
bytes for the 4 KiB-cluster default.

### Fix (`src/mkfs.rs`)

```rust
let volume_bytes = cluster_count * cluster_size as u64;
let backup_boot_byte_offset = volume_bytes - bytes_per_sector as u64;
dev.write_all_at(backup_boot_byte_offset, &boot)?;
```

The whole last cluster remains bitmap-allocated.

### Belt-and-suspenders attempt that backfired

A first attempt wrote the boot at BOTH positions
(start-of-last-cluster AND last-sector). That worked for 32 MiB but
broke 256 MiB — Event 55 fired at large volumes when two valid boot
signatures coexisted near the volume tail. Last-sector-only is the
correct answer for both:

| mkfs writes backup at         | 32 MiB chkdsk          | 256 MiB chkdsk        |
|-------------------------------|------------------------|-----------------------|
| start-of-last-cluster (only)  | Event 55, mount refuse | clean to frs.cxx 60f  |
| last-sector (only) — fix      | clean to frs.cxx 60f   | clean to frs.cxx 60f  |
| both positions                | clean                  | Event 55, mount refuse |

### Justification

Publicly documented NTFS layout: backup boot at the last 512-byte
sector of the volume, addressable as
`NumberSectors * bytes_per_sector`. Empirical: 8 scenarios
(`tiny-32mib`, `small-64mib`, `basic-256mib`, `large-1gib`,
`label-empty/32chars/cjk/latin1`) all reach the same residual
chkdsk state post-fix.

### Iteration

iter15 in [chkdsk-findings.md](./chkdsk-findings.md). Commits
[80a3d88, 2165997].

## Bug 8 — `ATTRS_OFFSET` hardcoded to 0x38 — broke writes against 4 KiB MFT records (iter16)

### Symptom

The new `write_ntfs` Mac CLI fails:

```
$ write_ntfs create vol.img / hello.txt
created file rec=24 //hello.txt
$ write_ntfs write  vol.img /hello.txt --content 'hi'
write_ntfs: write /hello.txt: unnamed $DATA attribute not found
```

### Per-byte diff (rec 24 just after `create_file` in a 4096-byte-record image)

| byte offset       | written value       | expected for 4096/512 |
|-------------------|---------------------|------------------------|
| 0x14 attrs_offset | 0x38                | **0x48**               |
| 0x38..0x42        | (attribute data)    | (USA[4..8] save-words) |
| 0x42..            | (zeros)             | (attribute data)       |

### Root cause

The USA region for a 4096-byte record at 512-byte sectors is 1 USN +
8 sector-saved-words = 18 bytes spanning 0x30..0x42. `record_build.rs`
hardcoded `const ATTRS_OFFSET: usize = 0x38`, which is INSIDE the
USA. `apply_fixup_on_write` overwrote the freshly-written attribute
bytes (at 0x38..0x42) with the saved sector-end words (zero-init).
The file's `$DATA` attribute literally disappeared.

`0x38` was correct only for 1024-byte records (sectors=2 →
align8(0x36) = 0x38), which is why `tests/write_root_ops.rs`
(uses 1024-byte fixture `test-disks/ntfs-basic.img`) didn't catch
this. mkfs.rs already computed `attrs_offset` per-record dynamically;
record_build.rs lagged.

### Fix (`src/record_build.rs`)

Replace the hardcoded constant with a per-record computation:

```rust
let attrs_offset = align8(USA_OFFSET + 2 + sectors * 2);
```

Apply at both call sites (`build_record_inner` for files,
`build_directory_record` for dirs). Same formula `mkfs.rs` already
uses.

### Justification

Pure layout arithmetic. USA spans `usa_offset..usa_offset + 2 +
sector_count * 2`; attrs must start past it, 8-byte aligned.

### Verification

End-to-end Mac smoke now passes:

```
mkfs → create /hello.txt → write 'hi' → mkdir /docs → create
/docs/notes.bin → write 256 bytes incrementing → inspect_ntfs lists
14 entries (11 system + /docs + /hello.txt + /docs/notes.bin) →
unlink /hello.txt → inspect_ntfs lists 13.
```

### Iteration

iter16 in [chkdsk-findings.md](./chkdsk-findings.md). Commit [9a640c5].

## Bug 9 — runtime `$FILE_NAME` `indexed_flag` zero on freshly created records (iter N, 2026-05-23)

### Symptom

`chkdsk DRIVE:` after a `mac:format → mac:mkdir foo → win:chkdsk`
sequence reports:

```
Stage 1: Examining basic file system structure ...
Attribute record (30, "") from file record segment 24 is corrupt.
Errors found.  CHKDSK cannot continue in read-only mode.
```

Matrix scenarios: `mac-format-mkdir-set-dirty-win-chkdsk`,
`mac-format-write-set-dirty-win-chkdsk`,
`mac-format-mac-write-win-repeat-mount-3-win-chkdsk`.

### Root cause

Bug 1 was fixed on the `mkfs` path (system records 0..0xB) but the
**runtime** code that synthesises new MFT records for
`fs_ntfs_create_file` / `fs_ntfs_mkdir` / hard-link / rename — i.e.
`src/record_build.rs` — had its own copy of the same wrong layout
and wrote `indexed_flag = 0` on every freshly created file's
`$FILE_NAME`.

### Fix

`src/record_build.rs::build_file_name_attribute` (`buf[v + 22] = 1`)
and `write_file_name` (`rec[at + 22] = 1`). Same byte, same value
as Bug 1 — just the second code path was missed.

### Iteration

Iter N. Commit [def4088].

## Bug 10 — runtime MFT `bytes_used` off by 4 (iter N, 2026-05-23)

### Symptom

After Bug 9 fixed, `chkdsk` reports:

```
First free byte offset corrected in file record segment 24.
```

on records produced by runtime `create_file` / `mkdir`.

### Root cause

Same off-by-four as Bug 2, in the second code path:
`src/record_build.rs::build_regular_file_record` and
`build_directory_record` both advanced `cursor` by 4 (just the
`0xFFFFFFFF` end-marker magic) instead of 8 (magic + the
`attribute_length = 0` u32 trailer) before computing `bytes_used`.

### Fix

Both builders now `cursor += 8` after writing the END marker.

### Iteration

Iter N. Commit [def4088].

## Bug 11 — `INDEX_HEADER.allocated_size_of_entries` not bumped on insert (iter N, 2026-05-23)

### Symptom

After Bugs 9–10 fixed, Windows Event Log emitted Event 55:

```
A corruption was found in a file system index structure on
volume %hs. The file reference number is N, the name is "...",
the index name is $I30, and the attribute type is $INDEX_ROOT.
```

on volumes that had had files/dirs added at runtime.

### Diagnostic

Byte-diff of `$INDEX_ROOT.value`'s `INDEX_HEADER`:

| Field                              | Pre-insert | Post-insert (ours) | Spec invariant |
|------------------------------------|------------|--------------------|----------------|
| `total_size_of_entries`   (+0x04)  | 0x4b8      | 0x518              | OK             |
| `allocated_size_of_entries` (+0x08)| 0x4b8      | **0x4b8**          | **must ≥ total** |

`allocated_size` was being left at its pre-insert value while
`total_size` grew.

### Root cause

`src/index_io.rs::insert_entry_into_index_root_with_collation`
updated `IH_TOTAL_SIZE_OF_ENTRIES` but not
`IH_ALLOCATED_SIZE_OF_ENTRIES`. NTFS spec requires
`allocated ≥ total`; chkdsk + ntfs.sys treat `allocated < total`
as corruption.

### Fix

Compute `new_size = total_size + entry_bytes.len()` once; write
it to both `IH_TOTAL_SIZE_OF_ENTRIES` and
`IH_ALLOCATED_SIZE_OF_ENTRIES`.

### Iteration

Iter N. Commit [def4088].

## Bug 12 — `$FILE_NAME.namespace` hardcoded to WIN32_AND_DOS (iter N, 2026-05-23)

### Symptom

`chkdsk DRIVE:` on a volume with a runtime-created file
`/persistent.txt` (stem 10 chars > 8) reported:

```
Stage 2: Examining file name linkage ...
An invalid filename persistent.txt (18) was found in directory 5.
All filenames for File 18 are invalid.
Minor file name errors were detected in file 18.
Index entry persistent.txt in index $I30 of file 5 is incorrect.
```

Matrix scenario: `mac-format-mac-write-win-repeat-mount-3-win-chkdsk`.

### Root cause

The MFT side of `$FILE_NAME` (in `src/record_build.rs`) was always
writing `namespace = 3` (WIN32_AND_DOS), which per MS-FSCC §2.4.4
requires the name to fit DOS 8.3 (stem ≤ 8 chars, ext ≤ 3 chars,
no extra dots). `persistent.txt` doesn't fit; chkdsk Stage 2
rejects the attribute.

### Fix

Add a helper `record_build::fn_namespace_for(name: &str) -> u8`
that returns `WIN32_AND_DOS` (3) when the name fits 8.3 and
`POSIX` (0) otherwise. Route `build_file_name_attribute` and
`write_file_name`'s callers through it.

This mirrors the rule already documented for the mkfs-side
`$Extend` children (`mkfs.rs::NAMESPACE_POSIX`).

### Iteration

Iter N. Commit [73a9a1c].

## Bug 13 — index entry's embedded `$FILE_NAME.namespace` disagrees with MFT side (iter N, 2026-05-23)

### Symptom

After Bug 12 fixed, the "invalid filename" error on the MFT-side
`$FILE_NAME` disappeared, but chkdsk Stage 2 still reported:

```
Index entry persistent.txt in index $I30 of file 5 is incorrect.
```

### Root cause

Each `$I30` entry embeds its own copy of the `$FILE_NAME` value.
After Bug 12 the MFT-record copy used POSIX (0) for non-8.3 names,
but `src/index_io.rs::build_file_name_index_entry` still
hardcoded `e[k + FN_NAMESPACE_OFFSET] = 3`. chkdsk validates that
the two copies agree.

### Fix

Same `record_build::fn_namespace_for(name)` helper used by Bug 12,
applied at the index-entry build site too.

### Iteration

Iter N. Commit [4f8bbdb].

## Outstanding — `frs.cxx 0x60f` chkdsk `/scan` ceiling and Windows write refusal

**State as of 2026-05-23**: after Bugs 1–13 fixed,
`chkdsk DRIVE:` (readonly) exits 0 cleanly across the matrix.
**`chkdsk DRIVE: /scan` still exits 13** — i.e. the ceiling
described below is still active, just on the `/scan` lane only.
The matrix `clean` verdict shape accepts
`readonly == 0 AND scan ∈ {0, 11, 13}` so the matrix passes;
tightening to `scan == 0` requires the working theory below to be
worked through.

The historical block below was written before Bugs 9–13 landed and
described the residual state of those iterations (`Stage 1 clean`,
`Stage 2 clean`, then internal assertion). The exact stdout has
since changed — current `/scan` exits 13 without printing the
`frs.cxx 60f` line in its captured stdout. The Procmon work
described in implementation-plan-secure-and-extend.md (Iter H) is
the most-promising path to identify what `/scan` actually keys on.

After Bugs 1–8 fixed, the eight scenarios `tiny-32mib`, `small-64mib`,
`basic-256mib`, `large-1gib`, `label-empty/32chars/cjk/latin1` all
reach the same residual state:

```
Stage 1: Examining basic file system structure ... [clean, 64 records]
Stage 2: Examining file name linkage ... [clean, 68 entries]
Index verification completed.
CHKDSK is scanning unindexed files for reconnect to their original directory.
An unspecified error occurred (6672732e637878 60f).
```

`6672732e637878 60f` decodes to ASCII `frs.cxx` (Microsoft's MFT-record
validation source file) at offset `0x60f` — an internal assertion
their pre-built chkdsk binary can fire but cannot describe. The
volume nonetheless mounts and reads.

**Independently** observed when wiring up the WinFixtures runner block
(iter18): Windows refuses **writes** to our volume:

```
Exception calling "WriteAllText" with "3" argument(s):
  "Insufficient system resources exist to complete the requested service."
```

(Win32 error 1450, `ERROR_NO_SYSTEM_RESOURCES`.) The volume mounts,
chkdsk reads it, but ntfs.sys won't accept user writes.

### Working theory (per-record diff against `format.com`)

Three structural omissions are likely candidates:

1. **`$SECURITY_DESCRIPTOR` (attribute type 0x50) absent on every
   system record.** Reference's records 0..10 each carry a 0x50
   resident attribute we don't write. Without an SD, ntfs.sys can
   neither resolve the file's ACL on access nor allocate a security
   token for a write.
2. **`$Secure`'s `$SDH` / `$SII` view indexes empty.** iter12 set the
   `MFT_RECORD_IS_VIEW_INDEX` flag bit but didn't populate the
   indexes. ntfs.sys may need the cache populated before it'll write
   security descriptors via `$Secure`.
3. **`$LogFile` filled with 0xFF.** Microsoft fills it with `RSTR`
   restart records and `RCRD` log records. Our 0xFF is a placeholder
   ntfs.sys treats as "log empty, force re-init" — but a write needs
   to *append* a log record, and there's no valid log header to
   append into.

A fix landing all three in one iteration should clear both
`frs.cxx 60f` and the write-refusal at the same time; that's the
next major mkfs initiative.

### Cluster-size axis bug catalog (iter17)

Each non-default cluster size surfaces a *different* chkdsk error.
The default-4096 path hides them; the matrix exercises them. Each
needs its own iteration:

| scenario      | cluster | chkdsk verdict                                                                  | likely cause |
|---------------|---------|---------------------------------------------------------------------------------|--------------|
| basic-256mib  | 4096    | clean to frs.cxx 60f                                                            | (baseline)   |
| cluster-512   | 512     | "Cannot open volume for direct access" — ntfs.sys refuses mount                 | $MFT placement at LCN 4 puts $MFT at byte 2048, immediately after the 512-byte boot; ntfs.sys may require more reserved space at small clusters |
| cluster-1k    | 1024    | "Corrupt master file table. CHKDSK aborted." — mounts but MFT structure invalid | similar boundary; or `clusters_per_mft_record` encoding (cpmr=4 for 4096-byte records / 1024-cluster) hits a validator quirk |
| cluster-8k    | 8192    | "Attribute record (80, $Bad) from file record segment 8 is corrupt." — Stage 1 | $BadClus's named "$Bad" sparse-run encoding may overflow a length field at 32768-cluster volumes, or ntfs.sys checks sparse attrs more strictly when cluster_size > 4096 |
| cluster-64k   | 65536   | "Incorrect information was detected in file record segment 5." — Stage 2        | 1 GiB / 65536-cluster gives only 16384 clusters; root-dir's $I30 with 12 entries may overrun the residency threshold (entries grow proportionally with name length, but $INDEX_ROOT total is capped by mft_record_size) |

### win-format scenarios (3 blocked)

Three scenarios use Microsoft `format.com` as the *primary* formatter
and exercise mac-side reads/writes:

- `win-format-win-write-mac-verify`
- `win-format-win-write-mac-write-win-verify`
- `win-format-win-write-mac-delete-win-verify`

The runner currently formats with `mkfs_ntfs` only; format.com is
the reference side. A `-Mode format-com` switch in
`scripts/run-windows-test.ps1` is needed to make these scenarios
runnable. Estimated 60 lines of PowerShell. Once added, those
scenarios should pass immediately because they don't depend on our
writer producing a writable volume.

## Tooling shipped alongside

- `src/bin/inspect_ntfs.rs` — Mac enumerate CLI (read-only walk).
- `src/bin/write_ntfs.rs` — Mac create/mkdir/write CLI.
- `src/bin/delete_ntfs.rs` — Mac unlink/rmdir CLI.
- `scripts/run-windows-test.ps1` — `-WinFixtures` block (PS write of
  test files into mounted volume; supports text, zeros, ones,
  incrementing patterns, and a `many:N:size` form for batch
  many-small-files), `-WinDelete` block, defensive wrapper.vhdx
  cleanup at start, MFT byte-offset computed from BPB instead of
  hardcoded 16384 (broke for non-default cluster sizes).
- `scripts/test-windows-local.sh` — `SSH_OPTS` env var (bypass broken
  ssh-agent), `CLUSTER_SIZE` plumbing, `WIN_FIXTURES`/`WIN_DELETE`
  conditional flag forwarding (empty values would otherwise be
  dequoted to bare `-WinFixtures` and crash PS).

## Source references used

For every bug above, "Justification" links to one of:

- **Microsoft MS-FSCC** ([MS-FSCC]: File System Algorithms) — the
  primary public spec. Authoritative for attribute layouts, BPB
  fields, $FILE_NAME body, security descriptors.
- **Microsoft `format.com /FS:NTFS`** output, dumped per-pipeline-run
  on the Windows ARM 11 VM. The actual byte stream a sanctioned
  Microsoft tool produces; differences from ours that affect chkdsk
  are by definition our bug.
- **Microsoft chkdsk's own diagnostic strings** — error names,
  internal-source-file offsets (e.g. `frs.cxx 60f`), event log
  messages. These are emitted by Microsoft's binaries; using them
  to triangulate which structure is wrong is observational, not
  reverse-engineering.

Sources NOT consulted (per project policy): any GPL-licensed
reverse-engineered Linux NTFS implementation, any Linux NTFS
project's documentation pages, any leaked or pirated Microsoft
internal source. Citations cite either Microsoft published material
or our own observed byte-diff against `format.com`.

## Cross-agent observations (per-worktree work-list survey)

The matrix has been run by multiple parallel agent sessions, each with
its own `tests/matrix/work-list.json`. Surveying every worktree
surfaces independent corroborations and a few useful divergences.
Sessions covered: `agent-5442`, `agent-840e`, `agent-8934`,
`agent-8a29`, `agent-c5fe`, `agent-c6a1` (this report's session).

### Convergent findings (multi-session corroboration)

- **iter13's root `$I30` populate fix is independently confirmed by 4
  agents.** `agent-5442` (commit `f3ea014`), `agent-8a29`
  (`6e203b9`), `agent-c5fe` (`1c5007a`), `agent-8934`
  (`7e87e87`) each independently produced essentially the same fix
  (collect entries during build, sort by COLLATION_FILE_NAME, emit
  populated INDEX_ROOT). All four converged on the same set of
  helpers (`build_file_name_stream`, `build_index_entry`,
  `build_populated_index_root_attr`, `collate_file_name`). Highest
  confidence in this fix.
- **iter14's BPB NumberSectors = N − 1 fix.** `agent-840e`
  (`41e601e`) ran the experiment and validated; `agent-c6a1`
  re-ran and confirmed (post-fix tiny-32mib still required iter15
  to mount, but the BPB field itself is correct).
- **The `frs.cxx 60f` ceiling is universal.** Every session that
  reached past Stage 2 hits it: marked
  `failed-frs-cxx-60f-tail-cycle3` (8934),
  `failed-needs-iter15-extend-or-secure` (c5fe),
  `blocked-chkdsk-ro3-scan11-pass6` (5442),
  `passed-to-frs60f-ceiling` (c6a1, 8a29). Same diag signature
  across all sessions: `An unspecified error occurred (6672732e637878 60f)`.
- **iter15 candidate consensus.** `agent-c5fe` ran an experiment
  emptying rec 11 (`$Extend`) to test whether `$Extend` was the
  cause of `frs.cxx 60f`: **not the cause**, error persists with
  rec 11 empty (commit `6ecf58c`: "iter14-v3 — rec 11 empty (no
  $Extend), confirms $Extend not the cause of frs.cxx:60f").
  Status entries across worktrees explicitly call out
  "needs-iter15-extend-or-secure" → narrowing to **$Secure view
  indexes** as the leading hypothesis.
- **Mac-side enumerate CLI works on every session that built one.**
  `agent-840e`, `agent-c5fe`, `agent-c6a1` all marked
  `mac-format-mac-enumerate-empty` as `passed-*`; the (empty
  expected) acceptance criterion is wrong post-iter13 — fresh
  volumes correctly list 11 system files.

### Divergent findings (cluster-size axis is flaky)

Different sessions reported different chkdsk verdicts for the same
cluster scenario, suggesting the failure modes shift with
runner-state, mount order, or shadow-copy availability:

| Scenario          | agent-c6a1 verdict          | agent-8934 verdict             | agent-c5fe verdict       |
|-------------------|------------------------------|--------------------------------|--------------------------|
| `cluster-512`     | failed-mount-refusal         | failed-corrupt-mft-at-cluster<4k | failed-needs-iter15-extend-or-secure |
| `cluster-1k`      | failed-corrupt-mft-1k        | failed-corrupt-mft-at-cluster<4k | failed-needs-iter15-extend-or-secure |
| `cluster-8k`      | failed-bad-attr-corrupt-8k   | failed-mount-collision         | failed-needs-iter15-extend-or-secure |
| `cluster-64k`     | failed-rec5-incorrect-64k    | failed-frs-cxx-60f-tail        | failed-needs-iter15-extend-or-secure |

Two interpretations:

1. The cluster-size scenarios run into multiple bugs at once; chkdsk
   reports whichever validator fires first, and that varies by
   scenario state. `agent-c5fe` may have run after iter15-related
   scaffolding (their pass3 dirs are timestamped after iter15
   discussion) and so saw the same residual `frs.cxx 60f` for
   everything; `agent-c6a1` saw the deeper per-cluster bugs
   first because they ran cleaner volumes.
2. The runner has cleanup races (stale wrapper.vhdx mounts from
   crashed prior runs blocking new mounts) that produce different
   surface symptoms. `agent-8934`'s `failed-mount-collision`
   verdict for cluster-8k is consistent with this — possibly a
   shared-VM-state issue, not an mkfs bug.

**Practical guidance**: when chasing cluster-size bugs, run each
scenario in isolation on a freshly cleaned VM workdir. Report the
verdict + Get-DiskImage state at start of run for repro.

### Per-session evidence directories (preserved on Mac)

All session diag dirs live under
`$TMPDIR/rust-fs-ntfs-diag/<session>/`. They contain per-iteration
`ours-boot.bin`, `ours-mft-16recs.bin`, `reference-boot.bin`,
`reference-mft-16recs.bin`, `chkdsk-readonly.txt`, `chkdsk-scan.txt`,
`eventlog-fs.txt`, `params-received.txt` (post-c6a1 only),
`win-fixtures-spec.txt` (post-c6a1 only). Index by session:

| Session     | Diag root                                                                   |
|-------------|----------------------------------------------------------------------------- |
| agent-c6a1  | `$TMPDIR/rust-fs-ntfs-diag/agent-c6a1-2026-05-02/iter-*`                    |
| agent-8934  | `$TMPDIR/rust-fs-ntfs-diag/agent-8934-2026-05-02/{iter-*,run-*.log}`        |
| agent-c5fe  | `$TMPDIR/rust-fs-ntfs-diag/agent-c5fe-2026-05-02/{iter-*,mac-only-pass3}`   |
| agent-840e  | `$TMPDIR/rust-fs-ntfs-diag/agent-840e-2026-05-02/`                          |
| agent-5442  | `$TMPDIR/rust-fs-ntfs-diag/agent-5442-2026-05-02/iter-*`                    |

Per-session reports (committed to `tests/matrix/agent-reports/`):
`agent-840e-2026-05-02.md`, `agent-8934-2026-05-02.md`,
`agent-8a29-2026-05-02.md`, `agent-c6a1-2026-05-02.md`. Each
narrates the session's specific iterations, bugs hit, fixes
attempted, and what they handed off.

### Pass-numbering convention used by sibling sessions

- `agent-5442` ran 6 passes through the work-list (`pass6` suffix
  in latest statuses), each pass re-running pending scenarios
  after previous-pass fixes landed.
- `agent-8934` ran cycles labelled `cycle1..cycle3`.
- `agent-c5fe` ran passes `pass1..pass3` plus a `mac-only-pass3`
  that exercised the Mac-only mac-enumerate scenario without
  touching the VM.

The "pass" / "cycle" / "iter" terminology is interchangeable —
all denote a sweep through the matrix re-running scenarios that
weren't yet `passed-*`.

### Sibling-agent code commits to know about

- `f3ea014` agent-5442: iter13 root $I30 populate (canonical)
- `8a94404` agent-5442: iter13 findings entry
- `41e601e` agent-840e: iter14 BPB NumberSectors fix + findings
- `48fb998` agent-840e: -ClusterSize runner plumbing
- `5721084` agent-c5fe: attempted iter16 — add $SECURITY_DESCRIPTOR
  to every system rec (REVERTED in `4cf548d` because it broke mount
  per-record-size-overflow)
- `06d53b4` agent-c5fe: bake Microsoft canonical NT 3.x $UpCase
  table (replace generator) — addresses the "Read-only chkdsk
  found bad on-disk uppercase table" warning
- `3fd37b7` agent-c5fe: $STANDARD_INFORMATION 48-byte (NTFS 1.x)
  form on system records — try smaller SI to make room for SD
- `6ecf58c` agent-c5fe: iter14-v3 — rec 11 empty (no $Extend),
  proves $Extend is NOT the cause of frs.cxx:60f
- `4ee3bad` agent-c5fe: FILE-magic placeholders for unused MFT
  slots 12..N-1 — partial mitigation for some chkdsk Stage 1
  errors
- `7e87e87` agent-8934: alternative iter13 root $I30 fix (not
  merged; superseded by 5442's)
- `5ba7c8d` agent-8934: orchestrator env-var plumbing (size + label)
- `c6a1` (this session): iter15 backup boot at last sector
  (`80a3d88`, `2165997`), iter16 ATTRS_OFFSET fix (`9a640c5`),
  inspect/write/delete CLIs (`fb54444`, `9a640c5`), iter17/18
  findings, WinFixtures runner.

### Branch / worktree cleanup

`agent-c6a1`'s worktree and branch were both removed after this
report. The session's commits were fast-forwarded into local `main`
(commits visible in `git log` from `5c88dea` backward). Nothing
pushed to `origin`. Other agents' worktrees (`agent-5442`,
`agent-840e`, `agent-8934`, `agent-8a29`, `agent-c5fe`,
`agent-ab9e...`) remain in `.claude/worktrees/` for the operator
to inspect or merge as desired.

## How to use this document for the next test pass

1. Re-run the matrix against current `main` (which carries Bugs 1–8
   fixed plus the cluster/frs60f/Windows-write findings documented
   here). Expected baseline:
   - 8 scenarios pass to the `frs.cxx 60f` ceiling
     (size + label axis on default cluster).
   - 4 cluster-size scenarios fail with the four distinct verdicts
     in the iter17 table.
   - 5 mac-format → win-write scenarios fail with
     `Insufficient system resources`.
   - 3 win-format scenarios remain blocked on the runner refactor.
2. Pick the next iteration target. Strongest leverage: implement the
   $SECURITY_DESCRIPTOR + $Secure + $LogFile bundle described under
   "Outstanding". Should clear `frs.cxx 60f` and win-write in one
   change.
3. If those clear: revisit cluster sizes (likely each fix is
   1–10 lines once the right field is identified per the iter17
   hypotheses).
4. Add `-Mode format-com` switch to runner; the 3 win-format
   scenarios should then run and probably pass on first attempt.

When in doubt: dump the bytes, diff against `format.com`. The
methodology in [chkdsk-findings.md](./chkdsk-findings.md) and
[multi-agent-test-protocol.md](./multi-agent-test-protocol.md) is
the runbook.

END-PROCESSED-INTO-chkdsk-improvement-findings -->

