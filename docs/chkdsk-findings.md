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

### iter13: root `$I30` was empty — populate with all 12 system files

**Symptom**

> Stage 1: Examining basic file system structure ... [clean]
> Stage 2: Examining file name linkage ...
>   68 index entries processed.
> Index verification completed.
> CHKDSK is scanning unindexed files for reconnect to their original directory.
> Detected orphaned file $MFT (0), should be recovered into directory file 5.
> Detected orphaned file $MFTMirr (1), should be recovered into directory file 5.
> [...all 12 system records 0..11...]
> Skipping further messages about recovering orphans.
> An unspecified error occurred (6672732e637878 60f).

(Verbatim from `rust-fs-ntfs-diag/agent-5442-2026-05-02/iter-20260502-024032/chkdsk-readonly.txt`,
captured pre-fix at iter13 by session `agent-5442-2026-05-02` against
the `mac-format-basic-256mib` scenario.)

**Diagnostic**

Local pipeline (`scripts/test-windows-local.sh`) Stage 1 cleared
post-iter12. Stage 2 reported every system record as orphaned —
chkdsk wants each file's parent's `$I30` to contain an `INDEX_ENTRY`
referencing it, but our root rec 5 shipped an empty index.

Decoded both `reference-mft-16recs.bin` (Microsoft `format.com`
output) and `ours-mft-16recs.bin`, comparing root rec 5's
`INDEX_ROOT '$I30'` attribute byte-for-byte:

**Per-field diff** *(rec 5 root `INDEX_ROOT '$I30'` attribute)*

| Field                  | reference | ours  | spec citation |
|------------------------|-----------|-------|---------------|
| INDEX_ROOT attr length | 0x488     | 0x50  | publicly published NTFS layout |
| value size             | 0x468     | 0x30  | same |
| INDEX_HEADER.entries_used | 0x458   | 0x20  | INDEX_HEADER struct |
| INDEX_ENTRY count      | 12 + LAST | LAST only | index walk |

Reference's 12 entries (sorted by COLLATION_FILE_NAME):
`$AttrDef`, `$BadClus`, `$Bitmap`, `$Boot`, `$LogFile`, `$MFT`,
`$MFTMirr`, `$Quota`, `$UpCase`, `$Volume`, `.`, plus LAST sentinel.
Each entry carries a `$FILE_NAME` stream byte-identical to that
record's in-record `$FILE_NAME` attribute value.

**Root cause**

NTFS requires every file's parent's `$I30` index to contain an
`INDEX_ENTRY` referencing the child via `(rec_num, sequence)` and
carrying the child's `$FILE_NAME` stream. The 12 system files all
declare `parent_reference = (rec=5, seq=5)`; the root therefore must
list all 12 (plus `.` itself, per Microsoft convention). Without
those entries, chkdsk Stage 2's "scanning unindexed files" walks the
MFT, finds in-use records whose parent claims to host them but
whose parent's `$I30` doesn't, and reports each as orphaned.

The cause was `build_empty_index_root_attr` literally building a
LAST-sentinel-only index. No mechanism existed to populate it.

**Fix** ([f3ea014])

`src/mkfs.rs`: collect `(rec_num, name, is_dir, data_alloc, data_real)`
during each rec 0..11 build (except rec 5 itself); move rec 5 build
to AFTER rec 11 so we have every system record's metadata. Sort by
COLLATION_FILE_NAME (ASCII upcase + UTF-16-LE bytewise — pure-ASCII
names match Microsoft's order). Emit one `INDEX_ENTRY` per record
carrying a `$FILE_NAME` stream byte-identical to the in-record one
(parent=(5,5), sequence=max(1, rec_num) per iter11, alloc/real per
iter9). Terminate with the LAST sentinel.

Helpers added:
* `build_file_name_stream` — reusable `$FILE_NAME` value bytes.
* `build_index_entry` — 16-byte header + stream + 8-byte align.
* `build_populated_index_root_attr` — wraps entries in `$I30` attr.
* `collate_file_name` + `ascii_upcase16` — COLLATION_FILE_NAME order.

**Result**

Post-fix Stage 2 output (`iter-20260502-025958/chkdsk-readonly.txt`):

> Stage 2: Examining file name linkage ...
>   68 index entries processed.
> Index verification completed.
> CHKDSK is scanning unindexed files for reconnect to their original directory.
> An unspecified error occurred (6672732e637878 60f).

The 12 orphan-recovery lines are GONE — every system record is now
properly indexed. Linux tests still pass (6/6). `cargo fmt --check`
clean; `cargo clippy --all-targets -- -D warnings` clean.

The remaining `frs.cxx 0x60f` internal error was *also* present in
iter12's post-fix output (after the orphan list, before chkdsk
truncated). Not introduced here; surfaced by the orphan flood being
peeled away. iter14 will tackle it.

### iter13b: corroboration on `mac-format-label-cjk` (CJK volume label)

Independent re-run of the iter13 fix on a different scenario by
session `agent-c6a1-2026-05-02`: the same 256 MiB / 4 KiB-cluster
volume but with `--label "日本語ラベル"` (CJK label, six BMP code
points). Two findings:

- **iter13 fix carries over verbatim.** Pre-fix
  (`rust-fs-ntfs-diag/agent-c6a1-2026-05-02/iter-20260502-025140`)
  showed the same orphan list (`$MFT (0)`...`$Secure (9)`) plus the
  `frs.cxx 60f` tail; post-fix
  (`iter-20260502-030838`) the orphan list is gone and only the
  `frs.cxx 60f` line remains. Same residual error, same chkdsk
  exit (3 readonly / 11 /scan), same shadow-copy snapshot warning.
  Two independent scenarios reaching the same post-fix state means
  the fix is not specific to the basic-256mib parameters.

- **The CJK label survives `mkfs.ntfs` UTF-16 encoding intact.**
  Decoded `$Volume`'s `$VOLUME_NAME` from
  `iter-20260502-030838/ours-mft-16recs.bin` (rec 3): exactly
  `E5 65 2C 67 9E 8A E9 30 D9 30 EB 30` — that's
  `日本語ラベル` in UTF-16-LE, byte-perfect. chkdsk's stdout
  rendering the label as `??????` is its own console-codepage
  issue (chkdsk pipes to a non-UTF-aware stream); the bytes on
  disk are correct.

Per the work-list, `mac-format-label-cjk` is therefore
`passed-implicitly-by-agent-5442-2026-05-02-c6a1` — same residual
state as the basic scenario, no label-specific bug introduced.

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
