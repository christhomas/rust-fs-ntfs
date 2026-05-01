# chkdsk findings — what Windows validation taught us

Running our `mkfs_ntfs` output through Microsoft `chkdsk` on Windows
surfaces structural bugs that pure-Linux round-trip tests miss. The
upstream `ntfs` reader crate is permissive about a number of NTFS
structures that Microsoft's own kernel + chkdsk are strict about.
This file records each bug Windows surfaced, the symptom, the
**evidence** for the diagnosis, and what we changed.

## How we corroborate fixes

We don't fix from hypothesis. We fix from **byte-level proof**: the
CI pipeline formats a second NTFS volume in parallel using
**Microsoft's own `format.com /FS:NTFS`** as the canonical reference,
then dumps the same byte ranges (boot sector, first 16 MFT records)
from both that reference volume and our `mkfs_ntfs` output. Any byte
that differs between the reference and ours, in a position that
matters to chkdsk, is **by definition** what we got wrong. The diff
is the proof.

The CI step that produces the proof is `Build a reference
Microsoft-formatted NTFS volume + diff against ours` in
`.github/workflows/ci.yml`. Its outputs land in the artifact:

- `reference-bpb.txt` — Microsoft's BPB decode (sector size, MFT
  location, etc.)
- `boot-sector-diff.txt` — full 512-byte boot sector, theirs vs ours
- `mft0-diff.txt` — first 4 KiB MFT record (`$MFT` itself), theirs
  vs ours
- `reference-format.txt` — full `format.com` transcript
- `reference-first-64k.bin` / `ours-first-64k.bin` — raw bytes for
  offline comparison

Comparing against publicly available NTFS layout descriptions (the
Microsoft-published MS-FSCC spec and the Windows Internals technical
reference) tells us *what each byte is supposed to mean*, but only
the byte-diff tells us *which byte we're actually getting wrong*.

## Why Linux tests aren't enough

Our Rust integration test (`tests/mkfs_roundtrip.rs`) reformats an
in-memory volume and parses it back with the upstream `ntfs` reader
crate. That confirms self-consistency — what we wrote, we can read —
but it does **not** verify Windows compatibility. Microsoft's NTFS
kernel driver and chkdsk are both stricter than the upstream reader,
and they're the validators that matter for shipping a real
filesystem to users.

The pipeline that catches this is `validate-mkfs-windows` in
`.github/workflows/ci.yml` — it boots `windows-latest`, builds
`mkfs_ntfs`, formats a volume, mounts it via VHDX, then runs both
read-only `chkdsk` and `chkdsk /scan` against it. The artifact
upload step preserves chkdsk's full output, fsutil dumps, Windows
Event Log, and pre/post-write byte dumps for every run.

## Iteration log

### iter1–iter2: VHDX wrapper plumbing (NOT mkfs bugs)

Symptom: Windows refused to assign a drive letter to a raw `.img`
mounted as a VHDX, even though the bytes were valid NTFS.

Root cause: Windows' VHDX mount path requires a **partition table** —
a raw NTFS volume at offset 0 (superfloppy layout) gets picked up on
physical media but not on VHDX-backed virtual disks. Confirmed by
`Get-Disk` reporting `NumberOfPartitions: 0  PartitionStyle: MBR`.

Evidence the bug was in CI, not mkfs: pre-wrap dump
(`nfs-img-bpb.txt`) showed our boot sector was structurally correct
(`bytes_per_sector=512, sectors_per_cluster=8, total_sectors=131072,
mft_lcn=4`). Bytes survived the dismount/remount round-trip
byte-identically.

Fix (CI only, not mkfs): wrap the raw image in a GPT-partitioned
VHDX. Create empty VHDX → `Initialize-Disk -PartitionStyle GPT` →
`New-Partition` aligned to 1 MiB → write our NTFS bytes into the
partition's offset on `\\.\PhysicalDriveN`. This is the same layout
`diskutil eraseDisk` produces on macOS, so the CI is closer to the
real shipping scenario.

### iter3: NTFS Event ID 55 — first proof of a real mkfs bug

Symptom: After all the wrapper plumbing was correct, `Get-Volume`
showed `FileSystemType: Unknown`, `Size: 0`, and `fsutil` returned
"Error 1393: The disk structure is corrupted and unreadable."

Diagnostic: dumped the Windows Event Log filtered to NTFS / Disk /
partmgr providers. Found 100+ entries of:

```
Provider: Ntfs   Event ID: 55   Level: Error
Message: A corruption was discovered in the file system structure on volume E:.
         The exact nature of the corruption is unknown.  The file system
         structures need to be scanned online.
```

Conclusion: Windows recognises our boot sector as NTFS but the kernel
detects corruption on every access to internal structures. Bug is in
mkfs_ntfs's MFT layout, not in the boot sector or the wrapper.

### iter4–iter6: getting chkdsk to actually run

`chkdsk /scan` failed at "Insufficient storage available to create
either the shadow copy storage file" on a 64 MiB volume. Bumped the
volume to 256 MiB (and the wrapper to 384 MiB to hold GPT slack).
Also hit secondary issues:

- `New-Partition` with explicit `-Offset 1MB -Size $rawSize` failed
  with "specified offset is not valid" because `Initialize-Disk`
  didn't refresh the cached `$disk.LargestFreeExtent`. Fix: re-fetch
  `Get-Disk` post-init and use `-UseMaximumSize`.

### iter6: chkdsk surfaces specific bugs

With the plumbing finally working, plain `chkdsk DRIVE:` (read-only,
no /scan) produced this output:

```
Read-only chkdsk found bad on-disk uppercase table - using system table.
Stage 1: Examining basic file system structure ...
Attribute record (30, "") from file record segment 0 is corrupt.
Attribute record (30, "") from file record segment 1 is corrupt.
[...repeats for segments 0..B (12 system files)...]
Errors found.  CHKDSK cannot continue in read-only mode.
```

Two distinct bugs were surfaced **but not yet diagnosed**:

#### Bug A: `$FILE_NAME` (attr type 0x30) corrupt on every system record

chkdsk reported `(30, "")` — the unnamed `$FILE_NAME` attribute — as
"corrupt" on records 0..0xB ($MFT, $MFTMirr, $LogFile, $Volume,
$AttrDef, root dir, $Bitmap, $Boot, $BadClus, $Secure, $UpCase,
$Extend). The upstream `ntfs` reader was happy to parse them; chkdsk
was not.

**Status: confirmed via byte-diff in iter8 (run id 25234929879).** The
CI step that formats a parallel Microsoft NTFS volume with
`format.com` and dumps each MFT record from both gave us the
ground truth. Per-record decode of `$FILE_NAME` (attribute type
0x30) on system records 0, 1, 5, 6, 10:

| Rec | Name        | namespace (ref/ours) | indexed_flag (ref/ours) | alloc/real (ref) | alloc/real (ours) |
|-----|-------------|----------------------|-------------------------|------------------|-------------------|
| 0   | `$MFT`      | 3 / 3 ✓             | **1 / 0 ✗**             | 0x10000 / 0x10000 | **0 / 0 ✗**       |
| 1   | `$MFTMirr`  | 3 / 3 ✓             | **1 / 0 ✗**             | 0x4000 / 0x4000   | **0 / 0 ✗**       |
| 5   | `.` (root)  | 3 / 3 ✓             | **1 / 0 ✗**             | 0 / 0 ✓           | 0 / 0 ✓           |
| 6   | `$Bitmap`   | 3 / 3 ✓             | **1 / 0 ✗**             | 0x3000 / 0x2E00   | **0 / 0 ✗**       |
| 10  | `$UpCase`   | 3 / 3 ✓             | **1 / 0 ✗**             | 0x20000 / 0x20000 | **0 / 0 ✗**       |

Two confirmed bugs:

1. **`indexed_flag` (attribute header offset 0x16) is 0 on every
   `$FILE_NAME`; Microsoft sets it to 1 on every one.** Every
   `$FILE_NAME` is referenced from the parent directory's `$I30`
   index — the `indexed_flag` byte at attribute-header offset 0x16
   advertises that fact. NTFS spec from publicly-published
   Microsoft references describes this byte as "Resident: indexed
   flag (1 if attribute referenced from index)." chkdsk verifies it
   against the structural reality.

2. **`$FILE_NAME`'s `allocated_size` and `real_size` (value bytes
   0x28..0x30 and 0x30..0x38) are 0 on every record; Microsoft
   populates them with the underlying `$DATA`'s allocated and real
   sizes.** Directories without `$DATA` (root, `$Volume`, `$Extend`,
   `$BadClus`'s unnamed `$DATA`) correctly have 0/0 in BOTH the
   reference and ours — the only difference is on records that have
   real `$DATA` content. chkdsk catches the inconsistency.

   Worth noting: the namespace byte (value offset 0x41) is `3`
   (WIN32_DOS) for `$MFT`, `$MFTMirr`, `.`, `$Bitmap`, `$UpCase` in
   *both* the reference and ours. So the system-files-need-POSIX
   theory was wrong — Microsoft uses WIN32_DOS for the `$`-prefixed
   names too. Glad we didn't change this without proof.

**Fix (iter9):**
- Set `rec[at + 22] = 1` (indexed_flag) in `write_file_name`.
- Add `data_alloc: u64, data_real: u64` parameters to
  `write_file_name` and `build_system_record`.
- Each of the 12 system-record call sites now passes the
  appropriate sizes:
  - `$MFT`: `mft_clusters * cluster_size` (= initial MFT data size)
  - `$MFTMirr`: `mftmirr_clusters * cluster_size`
  - `$LogFile`: `logfile_clusters * cluster_size`
  - `$Volume`: 0 / 0 (empty `$DATA`)
  - `$AttrDef`: `attrdef_clusters * cluster_size` / `attrdef_blob.len()`
  - root dir: 0 / 0 (no `$DATA`)
  - `$Bitmap`: `bitmap_clusters * cluster_size` / actual bitmap_bytes
  - `$Boot`: `boot_clusters * cluster_size` / 8192
  - `$BadClus`: 0 / 0 (unnamed `$DATA` is empty)
  - `$Secure`: 0 / 0
  - `$UpCase`: `upcase_clusters * cluster_size` / 131072
  - `$Extend`: 0 / 0

#### Bug B: bad on-disk uppercase table (warning, not fatal)

Symptom: "Read-only chkdsk found bad on-disk uppercase table - using
system table."

Likely root cause: `src/upcase.rs::generate_upcase_table` builds the
`$UpCase` mapping using Rust stdlib's `char::to_uppercase()`, which
follows current Unicode case rules. Microsoft's NTFS uses a specific
historical uppercase table (NT 5+ era) that's slightly different
from current Unicode mapping. chkdsk falls back to its built-in
table when the on-disk table doesn't match the canonical NT version.

**Status: filed for later.** This is a chkdsk warning, not a fatal
error — chkdsk continues and treats the volume as functional, just
using its own internal table for collation comparisons. Will be
fixed once Bug A is resolved.

The right fix is to ship the canonical NT5+ `$UpCase` table contents
as a `const` byte array generated independently from publicly
documented NTFS specs (Microsoft's MS-FSCC and NTFS technical
reference). Since the table is 128 KiB of fixed data, the cleanest
approach is to derive it from a byte-diff against a Microsoft-
formatted volume (Bug B will be self-resolving once we have the
reference dump from iter7+).

### iter9: $FILE_NAME fixes confirmed; new bug surfaced

Output after applying iter9's fixes (`indexed_flag=1` + populated
`$FILE_NAME` sizes):

```
Read-only chkdsk found bad on-disk uppercase table - using system table.
Stage 1: Examining basic file system structure ...
First free byte offset corrected in file record segment 0.
First free byte offset corrected in file record segment 1.
Incorrect information was detected in file record segment 2.
First free byte offset corrected in file record segment 2.
[...all 12 system records have "First free byte offset corrected"...]
Errors found.  CHKDSK cannot continue in read-only mode.
```

**The "Attribute record (30, '') is corrupt" errors are GONE.** Both
the indexed_flag and `$FILE_NAME` size fixes were correct.

The new error class — "First free byte offset corrected" — is
chkdsk fixing the `bytes_used` field in the MFT record header
(record offset 0x18). Per-record byte-diff:

| Rec | ref bytes_used | ref end_marker_at | ours bytes_used | ours end_marker_at |
|-----|----------------|-------------------|-----------------|--------------------|
| 0   | 0x210          | 0x208             | 0x17C           | 0x178              |
| 1   | 0x1D0          | 0x1C8             | 0x164           | 0x160              |
| 5   | 0x680          | 0x678             | 0x15C           | 0x158              |
| 11  | 0x130          | 0x128             | 0x164           | 0x160              |

**Pattern:** Reference always sets `bytes_used = end_marker_offset + 8`.
Ours always sets `bytes_used = end_marker_offset + 4`.

Reason: the NTFS attribute end marker is **8 bytes** total —
type=0xFFFFFFFF (4 bytes) followed by a length=0 field (4 bytes) —
not 4 bytes. The trailing 4 bytes happen to be zero in our
zero-initialised buffer, so the *content* matches; but our cursor
advance and `bytes_used` calculation didn't account for them.

**Fix (iter10):** in `build_system_record`, advance cursor by 8
after writing the end marker (was 4). Comment + cite the iter9
diff in source so the next reader knows why.

Note: ref's bytes_used values are *larger* than ours independent
of the +4 — that's because Microsoft writes additional attributes
(notably `$SECURITY_DESCRIPTOR` on `$MFT`) that we don't. The +4
fix makes our `bytes_used` self-consistent with our cursor; it
doesn't make our records identical to Microsoft's. chkdsk's
complaint is only about self-consistency.

### iter10: bytes_used fix worked; seq mismatch surfaced

After iter10's `bytes_used += 8` fix, the "First free byte offset
corrected" errors are gone. Records 0 and 1 are CLEAN. But records
2..0xB still report "Incorrect information was detected in file
record segment N".

Asymmetry was the clue: why are 0 and 1 fine but 2-11 broken? Per-
record dump of `sequence` field (record offset 0x10) and the
`parent_reference` inside `$FILE_NAME`:

| Rec | ref seq | ours seq | parent_ref (both) |
|-----|---------|----------|-------------------|
| 0   | 1       | 1        | (rec=5, seq=5)    |
| 1   | 1       | 1        | (rec=5, seq=5)    |
| 2   | 2       | **1**    | (rec=5, seq=5)    |
| 3   | 3       | **1**    | (rec=5, seq=5)    |
| 4   | 4       | **1**    | (rec=5, seq=5)    |
| 5   | 5       | **1**    | (rec=5, seq=5)    |
| 6   | 6       | **1**    | (rec=5, seq=5)    |
| ... | ...     | **1**    | (rec=5, seq=5)    |
| 11  | 11      | **1**    | (rec=5, seq=5)    |

**Microsoft sets sequence_number = max(1, rec_number)** for system
records. Specifically the root directory at rec 5 has seq=5. All
system files' `parent_reference` points to (5, 5) — i.e. "root,
sequence 5". With OUR seq always = 1, the root dir at rec 5 has
seq=1, which doesn't match the (5, 5) pointer the children
claim. chkdsk catches this inconsistency.

Records 0 and 1 happen to be clean because their seq=1 matches the
constant we wrote. Everything else fails the parent-reference
sanity check.

**Fix (iter11):** in `build_system_record`, `seq = max(1, rec_num)`.

## What we learned

1. **Microsoft's NTFS implementation is the only authoritative
   validator.** Linux NTFS readers are permissive about fields that
   Windows is strict about — specifically namespace selection,
   indexed_flag, and various `$FILE_NAME` consistency invariants.

2. **`chkdsk DRIVE:` (read-only) gives more useful diagnostic
   output than `chkdsk /scan`** on a small volume. /scan needs
   shadow-copy storage which fails on volumes under ~256 MiB.

3. **NTFS Event ID 55 in the Windows Event Log is the earliest
   signal of mkfs bugs** — it fires before chkdsk runs, on every
   kernel access to a corrupt volume. Worth capturing in CI even
   when chkdsk runs.

4. **The CI iteration loop is fast enough to iterate productively**
   (~2 minutes per push tag → result), but a local Vagrant Windows
   VM would shrink each iteration to ~10s. Worth setting up if the
   bug count grows.

5. **Don't fix bugs that don't exist.** iter1–iter5 chased VHDX
   wrapper plumbing issues that looked like mkfs bugs (Windows
   reporting "disk corrupt") but were actually CI-side problems
   with how we present the volume to Windows. The pre-wrap byte
   dump (`nfs-img-bpb.txt`) was the diagnostic that proved
   `mkfs_ntfs` was producing structurally correct boot sectors —
   it isolated the wrapper layer from the mkfs layer.

6. **Fix from byte-diff, not from hypothesis.** When chkdsk says
   "corrupt" without saying which byte, run the same code path
   through Microsoft's own `format.com` and diff the bytes. The
   bytes that differ in chkdsk-relevant positions are the actual
   bug. Reading the public NTFS layout spec (MS-FSCC) tells you
   what each byte means; the diff tells you which one we got wrong.
