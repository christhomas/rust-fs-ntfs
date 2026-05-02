# chkdsk findings — what Windows validation taught us

Running our `mkfs_ntfs` output through Microsoft `chkdsk` on Windows
surfaces structural bugs that pure-Linux round-trip tests miss. The
upstream `ntfs` reader crate is permissive about a number of NTFS
structures that Microsoft's own kernel + chkdsk are strict about.
This file records each bug Windows surfaced, the symptom, the
**evidence** for the diagnosis, and what we changed.

## How we corroborate fixes

We don't fix from hypothesis. We fix from **byte-level proof**: the
pipeline formats a second NTFS volume in parallel using
**Microsoft's own `format.com /FS:NTFS`** as the canonical reference,
then dumps the same byte ranges (boot sector, first 16 MFT records)
from both that reference volume and our `mkfs_ntfs` output. Any byte
that differs between the reference and ours, in a position that
matters to chkdsk, is **by definition** what we got wrong. The diff
is the proof.

Two iteration backends produce the same `diag/` artifact:

- **Local** (preferred during active iteration, ~30-90s per cycle):
  `scripts/test-windows-local.sh` runs the full pipeline against a
  Windows ARM64 VM over SSH. See [local-test-pipeline.md](./local-test-pipeline.md).
- **CI** (used for PR validation, ~2-4 min per cycle):
  `validate-mkfs-windows` job in `.github/workflows/ci.yml`.

Both produce the same `diag/` outputs:

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

### iter12: $Secure flags

**Symptom**

> Stage 1: Examining basic file system structure ...
> 64 file records processed.
> File verification completed.
> 0 large file records processed.
> 0 bad file records processed.
> Flags for file record segment 9 are incorrect.
>
> Stage 2: ...
> CHKDSK is scanning unindexed files for reconnect to their original directory.
> Detected orphaned file $MFT (0), should be recovered into directory file 5.
> [...orphan list runs 0..9 then stops...]

(Verbatim from the local-pipeline diag dir
`$TMPDIR/rust-fs-ntfs-diag/iter-20260502-014556/chkdsk-readonly.txt`,
captured pre-fix on this iteration.)

**Diagnostic**

Ran `bash scripts/test-windows-local.sh` (with
`VM_WORKDIR=C:/Users/chris/dev/rust-fs-ntfs-a` to avoid colliding with
a sibling agent worktree). The pipeline:

1. Builds `mkfs_ntfs.exe` on the Windows ARM64 VM.
2. Formats `nfs.img` with our mkfs.
3. In parallel, formats a 256 MiB reference VHDX with Microsoft's own
   `format.com /FS:NTFS`.
4. Dumps the first 16 MFT records from each into
   `ours-mft-16recs.bin` / `reference-mft-16recs.bin`.
5. Wraps ours in a GPT VHDX, mounts, runs `chkdsk` read-only.

Parsed both 16-record dumps with `python3 -c struct.unpack` to get
field-level decode of every system record. Stride is 4096 in both
files (one record per MFT_RECORD_SIZE-aligned slot).

**Per-field diff** *(rec 9 MFT record header bytes 0x00..0x48)*

| Offset | Field          | reference | ours | diff |
|--------|----------------|-----------|------|------|
| 0x00   | magic          | `FILE`    | `FILE` |   |
| 0x04   | usa_offset     | 0x30      | 0x30 |    |
| 0x06   | usa_count      | 0x09      | 0x09 |    |
| 0x10   | sequence       | 0x09      | 0x09 |    |
| 0x12   | link_count     | 0x01      | 0x01 |    |
| 0x14   | attrs_offset   | 0x48      | 0x48 |    |
| **0x16** | **flags**    | **0x0001** | **0x0001** | **identical** |
| 0x18   | bytes_used     | 0x0198    | 0x0130 | layout (ref has $SD attr we don't write) |
| 0x1C   | bytes_allocated | 0x1000   | 0x1000 |    |
| 0x28   | next_attr_id   | 0x04      | 0x10 | initialiser choice |
| 0x2C   | mft_rec_num    | 0x09      | 0x09 |    |

The flag byte at 0x16 is **identical** between reference and ours.
The corroboration mechanism is **uninformative for this field on this
record** because the reference's rec 9 is structurally a different
file than ours:

| rec | reference $FN  | ours $FN   |
|-----|---------------|------------|
| 8   | `$BadClus`    | `$BadClus` |
| **9** | **`$Quota`** | **`$Secure`** |
| 10  | `$UpCase`     | `$UpCase`  |
| 11  | (no $FN)      | `$Extend`  |

Microsoft's modern `format.com` lays `$Secure` somewhere outside the
first 16 records (likely under `\$Extend\$Secure`) and parks `$Quota`
at the historic NTFS-3.0 `$Secure` slot (rec 9). chkdsk reads our
`$FILE_NAME` and identifies our rec 9 *by name* as `$Secure`
(confirmed by the orphan-recovery line `Detected orphaned file
$Secure (9)…` in the same chkdsk run), so its expectations for the
flags field are keyed on the name, not the slot.

**Root cause**

Per Microsoft MS-FSCC field references for
`_FILE_RECORD_SEGMENT_HEADER.Flags`, the MFT record header `Flags`
field at offset 0x16 carries:

- 0x0001 `MFT_RECORD_IN_USE`
- 0x0002 `MFT_RECORD_IS_DIRECTORY` (record hosts an `$I30`
  $FILE_NAME index — i.e. an ordinary directory)
- 0x0004 reserved / "is 4"
- 0x0008 `MFT_RECORD_IS_VIEW_INDEX` (record hosts a *named view
  index* — anything indexing something other than `$FILE_NAME`,
  e.g. `$Secure`'s `$SDH`/`$SII`, `$Quota`'s `$O`/`$Q`,
  `$ObjId`'s `$O`, `$Reparse`'s `$R`)

`$Secure` is the canonical view-index host: it's a security
descriptor cache backed by two named indexes (`$SDH` keyed on hash,
`$SII` keyed on security ID). chkdsk has hardcoded knowledge of
`$Secure` and demands the `IS_VIEW_INDEX` bit on its MFT header
even when the on-disk view-index attributes are absent (our v1 ships
an empty stub with just an empty `$DATA`).

Why the byte-diff doesn't show this: `format.com`'s rec 9 is
`$Quota`, not `$Secure`. Since `$Quota` is *also* historically a
view-index host but format.com's stub of it doesn't yet carry the
view indexes either, format.com leaves `flags=0x0001` and chkdsk
apparently doesn't check `$Quota` as strictly. The diff is between
two different files, each unflagged, so no flag-bit diff exists to
read off. The fix is keyed on the public NTFS-layout description of
the `$Secure` system file rather than a flag-byte diff against the
reference.

**Fix**

`src/mkfs.rs`: in `build_system_record`, set
`MFT_RECORD_IS_VIEW_INDEX (0x0008)` on rec 9 only.

```rust
let is_view_index = record_number == rec::SECURE;
let flags: u16 = 0x0001
    | if is_dir { 0x0002 } else { 0x0000 }
    | if is_view_index { 0x0008 } else { 0x0000 };
```

A multi-line code comment near the change cites this iteration's diag
dir and the public spec, so the next reader knows why rec 9 is
special-cased.

Strict scope: only rec 9 changes. The other 11 system records keep
their previous flags values — no other chkdsk error has yet pointed
at them, and the corroboration mechanism doesn't justify a wider
change.

**Result**

To be verified by the merging step. Linux tests pass (`cargo test
--release --lib mkfs --test mkfs_roundtrip --test mkfs_bin_smoke`),
`cargo fmt --check` clean, `cargo clippy --all-targets -- -D
warnings` clean. The next iteration's chkdsk run on the Windows VM
will tell us whether (a) `Flags for file record segment 9 are
incorrect` is gone, and (b) whether a deeper chkdsk error now
surfaces — chkdsk previously stopped Stage 1 at this error, so any
errors hidden behind it on rec 9 (and orphan-recovery messages for
rec 10+, which were truncated in the iter11 run) will only become
visible after this fix.

### iter13 (agent-c5fe-2026-05-02): root $I30 was empty — every system file was reported as orphaned

**Symptom**

Post-iter12 chkdsk output (diag dir
`$TMPDIR/rust-fs-ntfs-diag/agent-c5fe-2026-05-02/iter-20260502-024129`):

> Stage 1: Examining basic file system structure ...
> 64 file records processed.  File verification completed.
> 0 large file records processed.  0 bad file records processed.
> Stage 2: Examining file name linkage ...
> 68 index entries processed.  Index verification completed.
> CHKDSK is scanning unindexed files for reconnect to their original directory.
> Detected orphaned file $MFT (0), should be recovered into directory file 5.
> Detected orphaned file $MFTMirr (1), should be recovered into directory file 5.
> Detected orphaned file $LogFile (2), should be recovered into directory file 5.
> Detected orphaned file $Volume (3), should be recovered into directory file 5.
> Detected orphaned file $AttrDef (4), should be recovered into directory file 5.
> Detected orphaned file . (5), should be recovered into directory file 5.
> Detected orphaned file $Bitmap (6), should be recovered into directory file 5.
> Detected orphaned file $Boot (7), should be recovered into directory file 5.
> Detected orphaned file $BadClus (8), should be recovered into directory file 5.
> Detected orphaned file $Secure (9), should be recovered into directory file 5.
> Skipping further messages about recovering orphans.
> An unspecified error occurred (6672732e637878 60f).

iter12's $Secure flag fix is confirmed working — Stage 1 reports zero
errors. The new symptom is at Stage 2: every system record's
$FILE_NAME points to (rec=5, seq=5) but root rec 5's $I30 contains
**no entries** beyond the LAST sentinel, so every child appears as an
orphan.

**Diagnostic**

Decoded rec 5's $INDEX_ROOT body from `ours-mft-16recs.bin` /
`reference-mft-16recs.bin` and dumped each $I30 entry side-by-side.

**Per-field diff** (root rec 5 $INDEX_ROOT)

| Field                     | reference        | ours (pre-fix) | spec |
|---------------------------|------------------|----------------|------|
| $INDEX_ROOT total length  | 0x488            | 0x50           | publicly documented NTFS layout |
| Index value total_size    | 0x458            | 0x20           | INDEX_HEADER total_size |
| Number of $I30 entries    | 11 (every system file + `.`) | 0 (LAST sentinel only) | NTFS directory layout |

**Root cause**

`build_empty_index_root_attr` was being used unconditionally for rec 5,
producing a directory whose $I30 contained only the LAST sentinel. The
12 system records that we wrote with `parent_reference=(rec=5,seq=5)`
therefore had no return-link from root, and chkdsk's Stage 2 reconcile
phase reports each as orphaned.

**Fix**

Add `build_root_index_root_attr` and `RootIndexChild`. Walk the 12
system records (records 0..11 plus `.` self-link, dropping the
duplicate by routing root through the same builder), sort by NTFS
COLLATION_FILE_NAME (case-insensitive UTF-16 — ASCII uppercase
suffices for these names, see the comment on `collate_filename`), and
emit one $I30 entry per child followed by the LAST sentinel.

The $FILE_NAME copy embedded in each $I30 entry zeros allocated_size,
real_size, and file_attributes — matching Microsoft's pattern (every
ref entry except $MFT has these as 0; the per-record $FILE_NAME is
authoritative). $MFT alone in ref has alloc=0x10000 real=0x10000
fa=0x6 in its $I30 entry; we leave ours zeroed too — the inconsistency
is one Microsoft chose to introduce, not one chkdsk requires.

**Result (partial — Stage 2 orphan messages gone, post-Stage-2 still
errors)**

Re-ran the local pipeline (diag dir
`$TMPDIR/rust-fs-ntfs-diag/agent-c5fe-2026-05-02/iter-20260502-030255`).
chkdsk now produces:

> Stage 1: ... 64 file records processed.  File verification completed.
> Stage 2: ... 68 index entries processed.  Index verification completed.
> CHKDSK is scanning unindexed files for reconnect to their original directory.
> An unspecified error occurred (6672732e637878 60f).

The 12 "Detected orphaned file …" lines are gone — Stage 2 no longer
reports any orphans, and "Index verification completed" passes clean.
**chkdsk now gets further than it ever has on our output**, but a new
internal error (`frs.cxx:60f`) surfaces in the post-Stage-2 phase.
Likely root causes (none yet corroborated by byte-diff):

1. Root rec 5 lacks a `$SECURITY_DESCRIPTOR` attribute. Reference's
   rec 5 carries an embedded SD of 248 bytes (val_len=0xF8). Without
   it, chkdsk may fail when it tries to compute or verify the DACL
   for root.
2. Reference rec 11 is *not* `$Extend` — it has no $FILE_NAME, just
   $STANDARD_INFORMATION + $SECURITY_DESCRIPTOR + $DATA. We use rec 11
   for `$Extend` and link it from root. chkdsk may have hardcoded
   knowledge of rec 11 as a non-$Extend slot.
3. `$Extend` belongs at a higher MFT record number with non-trivial
   children ($ObjId, $Quota, $Reparse, $UsnJrnl) that we don't write.

Adding a default $SECURITY_DESCRIPTOR is the next iteration's task —
it's spec-citable (MS-FSCC SECURITY_DESCRIPTOR layout) and self-
contained — but bigger than one byte-diff fix, so it's deferred.

Linux test contract held throughout: `cargo test --release --lib mkfs
--test mkfs_roundtrip --test mkfs_bin_smoke` passes (6/6) before and
after. `cargo fmt --check` and `cargo clippy --all-targets -- -D
warnings` clean. Scenario `mac-format-basic-256mib` is therefore
marked `failed-needs-iter14-sd-<session>` rather than `passed-*`.

### iter14 (agent-c5fe-2026-05-02): default $SECURITY_DESCRIPTOR on root rec 5 — UNVERIFIED, VM dropped SSH auth

**Symptom**

Carried over from iter13: chkdsk's post-Stage-2 reconnect-scan errors
out with `An unspecified error occurred (6672732e637878 60f)` after
Stage 1 + Stage 2 both verify clean. Same wall on every mac:format
variant in the work-list (volume sizes 32M..1G, cluster sizes
512..64K, label encodings ASCII/Latin-1/CJK/empty/max-length, and
operation sequences with chkdsk-only or with Win-side write/delete
legs added). One root cause for 17 scenarios.

**Diagnostic**

Reference rec 5 (Microsoft format.com) carries an embedded
$SECURITY_DESCRIPTOR attribute of 248 bytes (val_len=0xF8, attr_len
=0x110); ours has none. The reference's SD is self-relative
(SE_SELF_RELATIVE | SE_DACL_PRESENT) with machine-specific owner
(BUILTIN\Administrators), group (local domain users), and an 8-ACE
DACL. With our root carrying no SD attribute and security_id=0 in
$STANDARD_INFORMATION (because $Secure is an empty stub on our v1),
chkdsk's post-Stage-2 walk has no security descriptor to consult for
root and crashes internally rather than failing gracefully.

This diagnosis is corroborated structurally (a real-NTFS field is
missing on our root), not byte-by-byte (we don't have a chkdsk run
post-fix because the VM dropped SSH auth between iter13 and iter14
and didn't recover within the working window).

**Per-field diff** *(root rec 5 attribute layout, post-iter13)*

| Attr type                | reference           | ours pre-iter14 | ours post-iter14 |
|--------------------------|---------------------|-----------------|------------------|
| $STANDARD_INFORMATION    | resident 0x48       | resident 0x60   | resident 0x60    |
| $FILE_NAME (`.`)         | resident 0x60       | resident 0x60   | resident 0x60    |
| $SECURITY_DESCRIPTOR     | resident 0x110      | **absent**      | **resident 0x70 (72-byte minimal SD)** |
| $INDEX_ROOT (`$I30`)     | resident 0x488      | resident 0x4E8  | resident 0x4E8   |

**Root cause (proposed; verification deferred)**

Per MS-DTYP and MS-FSCC, every NTFS file is required to carry either
an embedded $SECURITY_DESCRIPTOR attribute or a security_id pointer
into $Secure that resolves to one. Pre-NTFS-3.0 ($Secure was
introduced in 3.0) every file embedded its own SD; modern NTFS uses
the centralised $Secure cache as an optimisation. With $Secure empty
and no per-file SD, root has no resolvable security descriptor.
chkdsk's reconnect-scan walks every record's $FILE_NAME/parent_ref
and checks the parent's ACL against process tokens to determine
whether to report the orphan-recovery message — without a parent SD
to evaluate against, it crashes rather than skipping the check.

**Fix**

Embed a 72-byte self-relative $SECURITY_DESCRIPTOR on root rec 5:

- 20-byte header: revision=1, control=`SE_SELF_RELATIVE | SE_DACL_PRESENT`,
  OffsetOwner=20, OffsetGroup=32, OffsetSacl=0, OffsetDacl=44.
- Owner SID @0x14: `S-1-5-18` (NT AUTHORITY\SYSTEM, 12 bytes).
- Group SID @0x20: `S-1-5-18` (same).
- DACL @0x2C: 8-byte ACL header + one ACCESS_ALLOWED ACE granting
  `S-1-1-0` (Everyone) full access (mask `0x001F01FF`).

Total = 72 bytes. Generic — independent of formatting machine — and
satisfies "the parent has *some* SD" without copying Microsoft's
specific SIDs. Wired through `build_resident_unnamed(
ATTR_SECURITY_DESCRIPTOR, 3, &ROOT_SECURITY_DESCRIPTOR)` ahead of the
$INDEX_ROOT attr, which moves to `attr_id=4` to keep IDs unique.

**Result — UNVERIFIED**

Linux contract holds (6/6 mkfs/round-trip/bin tests pass; cargo fmt
+ clippy --all-targets -- -D warnings clean). The Windows VM at
`chris@192.168.213.145` started rejecting key auth (`Permission
denied (publickey,password,keyboard-interactive)`) shortly after
iter13's diag artefacts landed — likely sshd's MaxAuthTries hit a
temporary block after the 23 back-to-back pipeline runs in pass 1.
ICMP also blocked but TCP/22 reachable, so the VM is up; auth alone
is failing.

iter14's chkdsk verdict therefore has no on-disk evidence yet. The
next agent (or this one on a future SSH-reachable run) should:

1. Run `bash scripts/test-windows-local.sh` against
   tests/matrix/work-list.json's `mac-format-basic-256mib`
   parameters (256 MiB / 4096 cluster / "CITEST").
2. Inspect `chkdsk-readonly.txt` for either:
   - **Clean**: `Windows has scanned the file system and found no problems`.
   - **Different error**: a new structural complaint we haven't seen.
   - **Same `frs.cxx:60f`**: our SD layout is wrong; iterate.
3. Re-classify all 17 `failed-needs-iter14-sd-*` scenarios
   accordingly.

If a chkdsk run shows the SD still triggers `frs.cxx:60f`, the
debug ladder is: dump `ours-rec5-sd.bin` and decode against the
public Microsoft SECURITY_DESCRIPTOR layout (MS-DTYP §2.4.6) for
self-consistency before changing it again.

**iter14 verification — REVERTED (broke mount before chkdsk could run)**

Once VM SSH was restored, ran the iter14 build through the local
pipeline (diag dir
`$TMPDIR/rust-fs-ntfs-diag/agent-c5fe-2026-05-02/iter-20260502-053623`).
The 72-byte SD attribute on rec 5 caused a NEW failure mode:

> chkdsk readonly output:
>   Cannot open volume for direct access.

`Get-Volume` showed `FileSystemType: FAT32` (instead of NTFS), and
the Event Log filled with NTFS Event ID 55 errors:

> A corruption was discovered in the file system structure on
> volume \\?\Volume{1a4cfdd9-…}. The exact nature of the corruption
> is unknown.  The file system structures need to be scanned and
> fixed offline.

So Windows rejected our volume at *mount* time — before chkdsk got
a chance to read anything. The byte-level SD content I built decoded
back via Python as structurally valid (revision=1, control=0x8004,
correct offsets, valid SIDs, ACL_REVISION=2, AceCount=1, AccessMask
=0x001F01FF). The structure being valid in isolation is necessary
but not sufficient — Windows additionally checks invariants we
haven't matched, e.g.:

- Reference rec 5 places the DACL **before** owner/group (OffsetDacl
  =0x14, OffsetOwner=0xCC, OffsetGroup=0xDC). Ours puts owner/group
  first (OffsetOwner=20, OffsetGroup=32, OffsetDacl=44). Both are
  formally legal per MS-DTYP §2.4.6 but Windows kernel apparently
  prefers / requires the reference layout.
- Reference's SD has **8 ACEs** with several ACE types (allow + audit
  + inherited). Ours has 1 ACE. A minimal SD may need additional
  control flags (e.g. SE_DACL_AUTO_INHERITED 0x0400) we didn't set.
- $STANDARD_INFORMATION's `security_id` (offset 0x34, NTFS 3.1) is
  always 0 in our writer; reference may set it to a concrete index
  into $Secure even when an explicit SD attribute is present, and
  the kernel cross-checks the two.

Reverted iter14's SD changes (commit follows). Falls back to iter13's
verified state: 17 scenarios at `failed-needs-iter14-sd-*`, with
post-Stage-2 `frs.cxx:60f` as the single remaining symptom. The
correct iter14 fix is materially harder than the byte-budget I
allowed it; the next iteration of this task should:

1. Capture a wider byte-diff: dump every byte of reference rec 5's
   SD attribute (offsets/sizes/control/all SIDs/all ACEs) and walk
   it against MS-DTYP §2.4.6, §2.4.5.1 (SID), §2.4.4 (ACL/ACE).
2. Build a `$SECURITY_DESCRIPTOR` attribute that matches reference's
   *layout order* (DACL before owner/group), plus SE_DACL_AUTO_
   INHERITED if reference sets it, plus the 8 standard ACEs (or
   reduce to a minimal valid 2-ACE SYSTEM+Administrators DACL with
   the right control flags).
3. Re-run the pipeline before claiming any fix.

**iter15: drop `$Extend` at rec 11 + cross-apply c6a1's BPB fixes**

Three changes bundled (committed `54dda31`):

- Stop writing `$Extend` at rec 11. Microsoft's reference rec 11 has
  no `$FILE_NAME`, no `$INDEX_ROOT` — just `$STANDARD_INFORMATION` +
  `$SECURITY_DESCRIPTOR` + `$DATA`. Rec 11 is now zeroed in the MFT
  buffer and not marked in `$MFT:$Bitmap`.
- Backup boot sector at the LAST 512-byte sector of the volume
  (cross-applied from agent-c6a1-2026-05-02 commits 80a3d88 +
  2165997). Was at start of last cluster; that's 7 sectors too early
  and triggered Event ID 55 on small volumes.
- BPB NumberSectors = volume_sectors - 1 (cross-applied from c6a1
  commit 84a83d7). Was N (full count); spec says N-1.

Result on default 256 MiB and 32 MiB tiny (diag dirs
`iter-20260502-072332` / `iter-20260502-072454`): both volumes
mount cleanly as NTFS, Stage 1 + Stage 2 verify clean, post-Stage-2
reconnect-scan still errors `frs.cxx:60f` (same ceiling). Tiny was
previously failing at MOUNT (Event 55, FAT32 misdetection); now
reaches the same ceiling 256 MiB has been hitting since iter13.

**iter16-attempt: 104-byte SD on every system record — REVERTED, broke mount**

Hypothesis from agent-8a29's notes in the shared work-list: every
system record needs an embedded `$SECURITY_DESCRIPTOR` attribute.
Reference dump confirmed: every system record (0..10) has SD:0x80
(value=104 bytes), and on records 0/1/2/4/5/6/7/8/10 the bytes are
**byte-for-byte identical** — a single canonical SD shared across
system records. Records 3/9/11 differ at offsets 32–33 / 52–53 (the
two ACE access masks: `0x0001009F` instead of `0x00120089`, i.e.
write+delete instead of read-mostly).

Implementation: extracted `SYSTEM_SECURITY_DESCRIPTOR` from the
reference, wired `build_system_record` to emit it on every system
record, removed the per-rec-5 SD path.

Result on default 256 MiB (diag dir `iter-20260502-073152`):

> chkdsk readonly: "Cannot open volume for direct access."
> Get-Volume: FileSystemType: Unknown
> Event Log: 99 NTFS Event ID 55 (corruption discovered, exact nature unknown)

Verified the SD bytes on rec 0/1/5 byte-by-byte: **identical to
reference**. Yet Windows rejected the volume at mount. Some
structural interaction we don't yet understand — likely involves
$STANDARD_INFORMATION's `security_id` field (we always set it to 0;
maybe Windows rejects per-rec SD when security_id=0 + $Secure stub
empty), or attribute-cursor placement when SD pushes the next attr
to a different alignment.

Reverted (this commit): `build_system_record` no longer emits SD.
The two const definitions (`ATTR_SECURITY_DESCRIPTOR`,
`SYSTEM_SECURITY_DESCRIPTOR`) are kept under `#[allow(dead_code)]`
for the iter17 redo to reuse without re-deriving them. Falls back
to iter15's verified state.

**iter17 ladder (next agent's task)**

The post-Stage-2 `frs.cxx:60f` ceiling has survived: iter13 (root
$I30), iter14-v2 (root SD), iter15 ($Extend drop + BPB fixes),
iter16-attempt (SD on every system rec). Open hypotheses:

1. **`$Secure` view-index attributes ($SDS / $SDH / $SII)**.
   Reference rec 9's $Secure has these populated; ours has only an
   empty `$DATA` stub. chkdsk's reconnect-scan may dereference them.
2. **`$STANDARD_INFORMATION.security_id`**. We write 0 everywhere.
   With per-record SD missing, this might be tolerated (iter15
   state); when SD is added but security_id stays 0, the kernel may
   detect the inconsistency (iter16 state). Test: add SD on every
   record AND set security_id to 1 (or some non-zero) and see
   whether mount survives.
3. **`$LogFile` RSTR records**. We fill with 0xFF and trust ntfs.sys
   to re-init. Modern ntfs.sys may demand at least one valid RSTR
   record header at the start.
4. **`$UpCase` canonical bytes**. chkdsk says "bad on-disk uppercase
   table" since iter6; falls back to system table. Possibly the
   reconnect-scan keys on something the system table reports
   differently from the on-disk one.

## What we learned

Restructured `ROOT_SECURITY_DESCRIPTOR` to put DACL right after the
20-byte header (offset 0x14) with owner/group at the tail (offsets
0x30, 0x3C), matching the reference layout. Same total length
(72 bytes), same content fields (one ACE granting Everyone full
access; SYSTEM as owner+group). Diag dir
`$TMPDIR/rust-fs-ntfs-diag/agent-c5fe-2026-05-02/iter-20260502-054925`.

Outcome:

- Volume **mounts cleanly** as NTFS (`get-volume` reports
  `FileSystemType: NTFS`; the FAT32 misdetection that iter14-v1
  caused is gone).
- chkdsk completes Stage 1 clean (64 file records, no
  "First free byte" / "is corrupt" messages) and Stage 2 clean (68
  index entries verified).
- Post-Stage-2 reconnect-scan still errors with
  `An unspecified error occurred (frs.cxx:60f)`.
- NTFS Event ID 55 still fires (100 entries) during the chkdsk run
  itself — the kernel logs the corruption-discovered event when
  chkdsk's internal assertion trips, even though chkdsk Stage 1 + 2
  reported clean.

So the missing $SECURITY_DESCRIPTOR was *not* the cause of the
post-Stage-2 error. iter14-v2's SD makes our root structurally
closer to reference but doesn't unblock the matrix. The actual
cause of `frs.cxx:60f` lies elsewhere — most likely in $Extend's
contents (reference rec 11 has no $FILE_NAME and is just $DATA, ours
has $Extend with empty $I30) or in $Secure's view-index
attributes ($SDS / $SDH / $SII) which our v1 stub omits.

Kept iter14-v2 in (structurally correct, no regression, sets up
iter15+ to attack the right cause without confounding factors).
The matrix's 17 mac:format scenarios remain at
`failed-needs-iter15-<...>`.

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
