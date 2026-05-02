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
### iter14: small-volume mount refusal — NumberSectors off-by-one is real but not the proximate cause

Session: `agent-840e-2026-05-02`. Diag dirs:
`$TMPDIR/rust-fs-ntfs-diag/agent-840e-2026-05-02/iter1-tiny-32mib/` (pre-fix)
and `.../iter2-tiny-32mib-fix-numbersectors/` (post-fix).

**Symptom**

Scenario `mac-format-tiny-32mib` (32 MiB / 4096 cluster / label `TINY`)
fails on Windows. `chkdsk DRIVE:` and `chkdsk DRIVE: /scan` both exit 3
with stdout:

```
Cannot open volume for direct access.
```

`Get-Volume` for the assigned drive letter shows:

```
FileSystem           :
FileSystemType       : Unknown
HealthStatus         : Healthy
OperationalStatus    : Unknown
Size                 : 0
SizeRemaining        : 0
```

I.e. the partition is exposed but ntfs.sys refuses to recognise it as
NTFS, so chkdsk has nothing to lock. The Windows Event Log produced
no NTFS provider entries against this drive letter (in contrast to
iter3, where Event ID 55 fired repeatedly). Different failure mode —
the kernel rejected the BPB outright before any per-record validator
ran.

**Diagnostic**

Ran `scripts/run-windows-test.ps1 -VolumeSizeMb 32 -WrapperSizeMb 96
-Label TINY` against `agent-840e-2026-05-02`'s isolated VM workdir.
Pulled `diag/` back; compared `ours-boot.bin` (512 B) vs
`reference-boot.bin` (Microsoft `format.com` on a 96 MiB VHDX). The
reference is wider than our 32 MiB volume, so size-relative fields
will always differ; the question is *which differences are
spec-violations* on our side, not just layout choices.

**Per-field diff** *(NTFS BPB, fields where ours and reference differ in form rather than just magnitude)*

| Offset | Field            | reference (96 MiB)     | ours (32 MiB pre-fix) | spec citation |
|--------|------------------|------------------------|-----------------------|---------------|
| 0x1C   | HiddenSectors    | 0x80 (= partition LBA) | 0                     | NTFS BPB carries the partition's start LBA so legacy boot loaders can self-locate. format.com sets it; mkfs is partition-agnostic so leaves it 0. Modern ntfs.sys does not appear to use it for mount. **Layout choice, not a bug.** |
| 0x28   | NumberSectors    | 0x2FEFF (= 196351 = N-1) | 0x10000 (= 65536 = N) | Microsoft's convention is `NumberSectors = volume_sectors - 1`; the trailing sector hosts the backup boot copy and is *not* counted as a data sector. Our value counted the backup-boot sector. **Provably wrong on our side.** |
| 0x30   | MftLcn           | 0x1FF5 (~middle of vol) | 0x4 (start of vol)    | Both within-spec; modern format.com places MFT mid-volume, mkfs places it early. **Layout choice, not a bug.** |
| 0x38   | Mft2Lcn          | 0x2 (early)            | cluster_count/2       | Both within-spec. **Layout choice, not a bug.** |

The only category-2 (provably wrong) difference is at offset 0x28. mkfs
still places the backup-boot copy at the *start* of the last cluster
(LCN cluster_count - 1), not at the very last sector — that's a
separate latent issue but no spec-cited evidence forces a change yet.

**Root cause (of the spec violation that was fixable in this iteration)**

`src/mkfs.rs:647` was computing
`total_sectors = cluster_count * cluster_size / bytes_per_sector` and
writing that whole figure to BPB.NumberSectors. The comment in the
source (`"Includes the very last sector which contains the backup
boot."`) shows the author was aware of the question but resolved it
the wrong way. Microsoft's published NTFS BPB convention, observable
in every `format.com` reference dump we have produced, treats
NumberSectors as the count of *data* sectors only — i.e. one less
than the partition's sector count.

**Fix**

`src/mkfs.rs:646-657`: rename the local from `total_sectors` to
`volume_sectors`, then write `number_sectors = volume_sectors - 1` to
BPB offset 0x28. Multi-line comment in source cites this iteration's
diag dir and the spec convention so the next reader knows why the
field is N-1 and not N.

Linux tests stay green:

```
cargo test --release --lib --test mkfs_roundtrip --test mkfs_bin_smoke
# 7 passed; 0 failed
cargo fmt --check  # clean
cargo clippy --all-targets -- -D warnings  # clean
```

**Result**

Pipeline re-run after the fix shows the BPB now correctly reads
`total_sectors=65535` (was 65536). However:

- `chkdsk DRIVE:` still exits 3 with `Cannot open volume for direct
  access.`
- `Get-Volume` still shows `FileSystemType: Unknown, Size: 0` on the
  newly-assigned drive letter.

So the NumberSectors off-by-one was a real spec violation worth
fixing, but it is **not** the proximate cause of Windows refusing to
mount a 32 MiB volume produced by mkfs. There is at least one further
small-volume-specific issue. The next iteration's evidence-gathering
should focus on:

1. **Backup boot sector placement.** mkfs writes it at byte
   `(cluster_count - 1) * cluster_size` (i.e. the *start* of the last
   cluster, sector 65528 in the 32 MiB case). Microsoft format.com is
   documented to place it at `volume_sectors - 1` (the very last
   sector, 65535). At 256 MiB the proportional misalignment was
   tolerated by ntfs.sys; at 32 MiB it may not be. To corroborate,
   read the reference's last 512 bytes (sector 196351 of the 96 MiB
   ref) and compare against our last cluster.
2. **MFT placement at LCN 4 on a 32 MiB volume.** Reference places
   MFT mid-volume; we place it at LCN 4 unconditionally. ntfs.sys
   may consult a heuristic at small volumes that rejects an
   early-MFT placement. Lower priority — needs evidence before
   action.
3. **`$LogFile` fixed at 64 KiB.** Microsoft's format.com scales
   `$LogFile` up to 1–4 MiB even on small volumes. A 64 KiB log may
   be below ntfs.sys's accepted minimum. Worth diffing the reference's
   `$LogFile` allocation against ours.

Status: scenario `mac-format-tiny-32mib` marked
`blocked-needs-evidence-32mib-mount-refusal-agent-840e-2026-05-02`.
NumberSectors fix retained on worktree branch
`agent/agent-840e-2026-05-02` for downstream agents to consume; not
pushed to main.

### iter15: backup boot sector at last sector, not start of last cluster

Session: `agent-c6a1-2026-05-02`. Picked up after iter14: 32 MiB still
refused to mount even with the BPB NumberSectors fix.

**Symptom**

NTFS Event ID 55 fired on every mount attempt of a 32 MiB volume:

> A corruption was discovered in the file system structure on volume
> \\?\Volume{...}.
> The exact nature of the corruption is unknown.  The file system
> structures need to be scanned and fixed offline.

`Get-Volume` reported `FileSystemType: Unknown`. `chkdsk DRIVE:`
emitted "Cannot open volume for direct access" and exited 3.

**Diagnostic**

Compared backup-boot location against publicly documented NTFS
layout. mkfs wrote the backup at byte
`(cluster_count - 1) * cluster_size` = start of the last *cluster*
(byte 33550336 for 32 MiB / 4096 cluster). Per the publicly published
NTFS layout, ntfs.sys reads `BPB.NumberSectors` and probes the byte
at offset `NumberSectors * bytes_per_sector` (= byte 33553920 = the
last 512-byte *sector* of the volume, not the start of the last
cluster). The two differ by 7 sectors / 3584 bytes.

**Per-position diff** *(mac-format-tiny-32mib post-iter14, pre-iter15
diag iter-20260502-054124)*

| byte offset                    | value      | who reads it       |
|--------------------------------|------------|--------------------|
| start-of-last-cluster (33550336) | boot copy  | (no consumer at small volumes — ntfs.sys ignores) |
| last-sector (33553920)         | zeros      | **ntfs.sys at small volumes — finds no signature → Event 55** |

**Root cause**

Pure layout error in mkfs's last write of the boot sector. Backup boot
must live at the actual last 512-byte sector of the volume.

**Fix** ([80a3d88], superseded by [2165997])

`src/mkfs.rs`: replace the write at
`backup_boot_lcn * cluster_size` with a write at
`(cluster_count * cluster_size) - bytes_per_sector`. The whole last
cluster is still bitmap-allocated.

A first attempt wrote at BOTH positions (belt-and-suspenders); that
broke `mac-format-basic-256mib` (Event 55 fired at >= 256 MiB when
two valid boot signatures coexisted near the volume tail). The
final fix writes at the last sector ONLY, which works for both
volume sizes:

| mkfs writes backup at         | 32 MiB chkdsk          | 256 MiB chkdsk        |
|-------------------------------|------------------------|-----------------------|
| start-of-last-cluster (only)  | Event 55, mount refuse | clean to frs.cxx 60f  |
| last-sector (only) — fix      | clean to frs.cxx 60f   | clean to frs.cxx 60f  |
| both positions                | clean                  | Event 55, mount refuse |

**Result**

mac-format-tiny-32mib now mounts cleanly. Same scenarios verified
post-fix: tiny-32mib, small-64mib, basic-256mib, large-1gib,
label-empty/32chars/cjk/latin1 — all reach the same residual chkdsk
state ("frs.cxx 60f" trailing internal error during Stage 2
orphan-recovery). Eight scenarios passed-to-ceiling on this fix.

Linux test contract (6/6) intact; pre-commit fmt + clippy clean.

### iter16: ATTRS_OFFSET hardcoded to 0x38 — broke writes against 4 KiB MFT records

Session: `agent-c6a1-2026-05-02`. Surfaced when the new
`write_ntfs` Mac CLI tried to write into a freshly-formatted volume.

**Symptom**

```
$ write_ntfs create vol.img / hello.txt
created file rec=24 //hello.txt
$ write_ntfs write  vol.img /hello.txt --content 'hi'
write_ntfs: write /hello.txt: unnamed $DATA attribute not found
```

`tests/write_root_ops.rs` did NOT catch this — those tests use a
1024-byte-record fixture (`test-disks/ntfs-basic.img`).

**Per-byte diff** *(rec 24 just after `create_file` in a 4096-byte-record image)*

| header field      | written value | expected for 4096/512 |
|-------------------|---------------|------------------------|
| `attrs_offset` (0x14) | 0x38          | 0x48                   |
| bytes 0x38..0x42  | (attribute data) | (USA[4..8] save-words) |
| bytes 0x42..      | (zeros)       | (attribute data)       |

The USA region for a 4096-byte record at 512-byte sectors is
1 USN + 8 sector-saved-words = 18 bytes spanning 0x30..0x42.
`record_build.rs:49` hardcoded `ATTRS_OFFSET = 0x38`, which is
*inside* the USA. `apply_fixup_on_write` then overwrote the
freshly-written attribute bytes (at 0x38..0x42) with the saved
sector-end words (zero-init). The file's $DATA attribute literally
disappeared. 0x38 happened to be correct only for 1024-byte records
(sectors=2 → align8(0x36) = 0x38), which is why the existing test
fixture passed.

**Root cause**

Pure layout error. mkfs.rs computes `attrs_offset` per-record
(`align8(USA_OFFSET + 2 + sectors * 2)`); record_build.rs used a
hardcoded constant for both `build_regular_file_record` and
`build_directory_record`.

**Fix** ([9a640c5])

`src/record_build.rs`: replace the constant with the same per-record
computation. Both call sites updated.

**Result**

End-to-end Mac-side smoke now passes: mkfs → write_ntfs create
/hello.txt → write 'hi' → mkdir /docs → create /docs/notes.bin →
write 256 bytes incrementing → inspect_ntfs lists 14 entries (11
system + /docs + /hello.txt + /docs/notes.bin) → unlink /hello.txt
→ inspect_ntfs lists 13. The Mac-side write/delete/enumerate CLIs
all work end-to-end against the freshly-formatted volume.

Linux test contract (6/6) intact.

### iter17: per-cluster-size bug catalog — non-default cluster sizes each surface a distinct mkfs bug

Session: `agent-c6a1-2026-05-02`. After iter15 unblocked the
volume-size axis, ran the cluster-size axis end-to-end. Each
non-default cluster size surfaces a *different* chkdsk error —
documenting the catalog so subsequent iterations can pick them off.

| scenario              | cluster | chkdsk verdict                                                                  |
|-----------------------|---------|---------------------------------------------------------------------------------|
| basic-256mib          | 4096    | clean to `frs.cxx 60f` ceiling                                                  |
| cluster-512           | 512     | "Cannot open volume for direct access" — ntfs.sys refuses mount                 |
| cluster-1k            | 1024    | "Corrupt master file table. CHKDSK aborted." — mounts but MFT structure invalid  |
| cluster-8k            | 8192    | "Attribute record (80, $Bad) from file record segment 8 is corrupt." — Stage 1 |
| cluster-64k           | 65536   | "Incorrect information was detected in file record segment 5." — Stage 2        |

The four cluster failures are real bugs in mkfs that the default
4096-cluster path doesn't exercise. Each will need its own iteration
to chase down — likely candidates per the byte layout:

- 512 cluster: MFT placement at LCN 4 puts $MFT at byte 2048,
  immediately after the boot's 512 bytes; ntfs.sys may require more
  reserved space at small clusters.
- 1k cluster: similar boundary issue, or `clusters_per_mft_record`
  encoding (cpmr=4 for 4096-byte records / 1024-cluster) hits a
  validator quirk.
- 8k cluster: $BadClus's named "$Bad" sparse run encoding may overflow
  a length field at 32768-cluster volumes, or ntfs.sys checks sparse
  attrs more strictly when cluster_size > 4096.
- 64k cluster: 1 GiB / 65536-cluster gives only 16384 clusters
  total; root-dir's $I30 with 12 entries may overrun the residency
  threshold (entries grow proportionally with name length, but the
  $INDEX_ROOT total is capped by mft_record_size).

Diag dirs (each contains the per-record dump pre- and post-mount):
- `iter-20260502-063326` (cluster-512)
- `iter-20260502-063421` (cluster-1k)
- `iter-20260502-063739` (cluster-8k)
- `iter-20260502-064211` (cluster-64k)

### iter18: Windows can't WRITE to our volumes — "Insufficient system resources"

Session: `agent-c6a1-2026-05-02`. Surfaced when wiring up the
WinFixtures runner block to support `mac:format → win:write` scenarios.

**Symptom** (PowerShell on Windows ARM64 against a freshly-formatted
256 MiB volume that mounts cleanly per iter15 + already passes Stage 1
+ Stage 2 chkdsk):

```
[3/6] Mounting + capturing diagnostics ...
  Mounted at E:
  Writing WinFixtures: tiny.txt=text:hello world
Exception calling "WriteAllText" with "3" argument(s):
  "Insufficient system resources exist to complete the requested service."
At ...run-windows-test.ps1:187 char:17
+ ...   [System.IO.File]::WriteAllText("${letter}:\$name", $value ...
  + FullyQualifiedErrorId : IOException
```

Win32 error 1450 (`ERROR_NO_SYSTEM_RESOURCES`). Equivalent failures
expected from `Set-Content`, `New-Item`, anything that asks ntfs.sys
to allocate buffers for a write.

**Root cause** (working theory)

Volume passes the *read* path (chkdsk reads it; Get-ChildItem on the
empty volume returns the system files). The write path requires
ntfs.sys to allocate from internal pools tied to `$Secure`'s
security-descriptor cache, $LogFile transactional state, etc. Our
writer ships:

- `$Secure` as a 9-byte resident stub (no `$SDH`/`$SII` view indexes,
  iter12 marked the flag but didn't populate the indexes).
- `$LogFile` filled with 0xFF (no `RSTR` / `RCRD` records).
- No `$SECURITY_DESCRIPTOR` (attr 0x50) on system MFT records
  (8934's iter15 candidate).

Without these, ntfs.sys can mount the volume read-only (or read-only
in effect) but refuses writes because it can't fault in an SD or
write a transaction record. `frs.cxx 60f` (the trailing chkdsk
error all our scenarios bottom out at) is the same family of issue
manifesting through chkdsk's deeper passes.

**Implication for the work-list**

Every `mac-format → win:write` scenario currently fails identically
("Insufficient system resources"). 5 scenarios marked
`failed-windows-cant-write-insufficient-resources-blocks-on-frs60f`.

Mitigation: the `win:format → win:write → mac:enumerate` family
SHOULD work since Microsoft's `format.com` produces a fully writable
volume. Those scenarios are blocked on the runner's lack of a
"primary format = format.com" mode (3 scenarios marked
`blocked-needs-winformat-mode-runner-refactor`).

Forward path: implement the `$SECURITY_DESCRIPTOR` + `$Secure`
indexes + valid `$LogFile` writers; this should unblock both the
chkdsk `frs.cxx 60f` ceiling AND the Windows-write resource error
in one go.

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

### iter13: orphan-system-files in root $I30

Session: agent-8a29-2026-05-02. Scenario: mac-format-basic-256mib.

**Symptom**

> Stage 2: Examining file name linkage ...
> 68 index entries processed.
> Index verification completed.
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

(Verbatim from local-pipeline diag dir
`$TMPDIR/rust-fs-ntfs-diag/agent-8a29-2026-05-02/iter-20260502-024137/chkdsk-readonly.txt`,
captured pre-fix on this iteration.)

**Diagnostic**

Ran `bash scripts/test-windows-local.sh` against worktree
`agent-8a29-2026-05-02` with `VM_WORKDIR=…rust-fs-ntfs-agent-8a29-2026-05-02`
to isolate the VM-side workdir. The pipeline:

1. Built `mkfs_ntfs.exe` on the Windows ARM64 VM.
2. Formatted `nfs.img` (256 MiB / cluster 4096 / label CITEST).
3. Formatted a parallel reference VHDX with `format.com /FS:NTFS`.
4. Dumped the first 16 MFT records from each into `*-mft-16recs.bin`.
5. Wrapped ours in a GPT VHDX, mounted, ran `chkdsk` read-only.

Parsed the reference's root `$INDEX_ROOT` ($I30) attribute with
`python3 struct.unpack`. The reference root index is 1128 bytes and
contains 11 leaf entries plus the LAST sentinel:

| e_off | entry_len | mft_rec | seq | name      |
|------:|----------:|--------:|----:|-----------|
| 32    | 104       | 4       | 4   | `$AttrDef`  |
| 136   | 104       | 8       | 8   | `$BadClus`  |
| 240   | 96        | 6       | 6   | `$Bitmap`   |
| 336   | 96        | 7       | 7   | `$Boot`     |
| 432   | 104       | 2       | 2   | `$LogFile`  |
| 536   | 96        | 0       | 1   | `$MFT`      |
| 632   | 104       | 1       | 1   | `$MFTMirr`  |
| 736   | 96        | 9       | 9   | `$Quota`    |
| 832   | 96        | 10      | 10  | `$UpCase`   |
| 928   | 96        | 3       | 3   | `$Volume`   |
| 1024  | 88        | 5       | 5   | `.`         |
| 1112  | 16        | (LAST)  | -   | sentinel  |

Ours (pre-fix) had only the LAST sentinel (48-byte $I30 attribute).
Every system MFT record was built with a `$FILE_NAME` whose
`parent_reference` is `(rec=5, seq=5)` (the root); chkdsk follows
each `$FN` back to root and looks for the name in root's $I30.
With root's index empty, *every* system file came up missing → the
"orphaned ... should be recovered into directory file 5" cascade.

**Per-field diff** *(rec 5 root, $INDEX_ROOT @ $I30)*

| Field                       | reference | ours (pre)  | spec citation |
|-----------------------------|----------:|------------:|---------------|
| $I30 attr content_size      | 1128      | 48          | MS-FSCC INDEX_ROOT |
| INDEX_HEADER.entries_offset | 16        | 16          | MS-FSCC INDEX_HEADER |
| INDEX_HEADER.index_length   | 1112      | 32          | MS-FSCC INDEX_HEADER (entries_offset + Σ entry_lengths) |
| Number of leaf entries      | 11        | 0           | observed |
| First entry file_ref        | (4,4)     | n/a         | $AttrDef per sort |
| Sort order                  | COLLATION_FILE_NAME | n/a | MS-FSCC §2.4 |

**Root cause**

Per the publicly documented NTFS layout (MS-FSCC INDEX_ROOT/
INDEX_HEADER/INDEX_ENTRY definitions), every entry in a directory's
$I30 is a `(file_reference, $FILE_NAME content)` pair sorted by
COLLATION_FILE_NAME. chkdsk's Stage 2 "scanning unindexed files for
reconnect" phase iterates all in-use MFT records and verifies each
record's $FN parent_reference can be resolved to an entry in the
parent directory's $I30. Records present in the MFT but absent from
the parent's $I30 are reported orphaned.

Our `mkfs_ntfs` populated each system record's $FN with
`parent_reference = (5, 5)` correctly but built root's $I30 as an
empty index. The mismatch was visible directly in the byte-diff:
reference root carried 1128 bytes of $I30 content; ours carried 48.

**Fix**

`src/mkfs.rs`: add a `SysIndexEntry` collector and a new
`build_index_root_attr_with_entries` that packs `(rec, seq, name,
is_dir, alloc, real)` tuples into a populated INDEX_ROOT. Move root
construction (rec 5) to after every other system record so all
data sizes are known. Sort the 12 entries (records 0..11 plus root
self) per COLLATION_FILE_NAME (case-insensitive UTF-16; ASCII
uppercase suffices for the pure-ASCII system file names). Each
index entry's $FN content mirrors the corresponding inline $FN
(parent_ref, timestamps, data sizes, file_attrs, namespace).

`tests/mkfs_roundtrip.rs::format_and_parse_back` updated: previous
`assert!(names.is_empty())` was asserting the buggy empty-root
behaviour; new assertion verifies the 12-entry sorted order
matches the publicly documented NTFS layout.

**Result**

Targeted error class — *all 10 "Detected orphaned file $X (N)"
messages* — eliminated. Post-fix chkdsk diag:
`$TMPDIR/rust-fs-ntfs-diag/agent-8a29-2026-05-02/iter-20260502-030328/chkdsk-readonly.txt`
shows Stage 1 + Stage 2 complete with 64 file records / 68 index
entries processed, no orphan messages, then chkdsk hits its
internal `frs.cxx` line 1551 assert (`An unspecified error
occurred (6672732e637878 60f)` — the hex decodes to `frs.cxx`).
That assert was already present in iter12; it is now the next
opaque error to investigate. The `Read-only chkdsk found bad
on-disk uppercase table - using system table` warning persists
and is also pre-existing — separate issue, separate iteration.

Linux baseline tests pass:
`cargo test --release --lib --test mkfs_roundtrip --test mkfs_bin_smoke`
all green (`mkfs::tests::run_encode_decode_roundtrip`,
`mkfs::tests::upcase_table_size`, `mkfs_bin_*`,
`format_and_parse_back`, `capi_mkfs_then_parse`). `cargo fmt
--check` clean. `cargo clippy --all-targets -- -D warnings` clean.

### iter14: $SECURITY_DESCRIPTOR (0x50) on every system MFT record

Session: agent-8a29-2026-05-02. Scenario: mac-format-basic-256mib (post-iter13).

**Symptom**

> An unspecified error occurred (6672732e637878 60f).

(Stage 2 error after orphan recovery, post-iter13. `6672732e637878` = ASCII "frs.cxx", followed by line 0x60f = 1551.)

**Diagnostic**

Parsed reference's first 16 MFT records (`reference-mft-16recs.bin` from iter13's diag dir) and found a **104-byte $SECURITY_DESCRIPTOR (attr type 0x50) on every system record** that ours did not have at all. Three unique SD blobs:

| Blob | Used by | Size | Distinguishing byte |
|------|---------|-----:|---------------------|
| RO  | $MFT, $MFTMirr, $LogFile, $AttrDef, $Bitmap, $Boot, $BadClus, $UpCase | 104 | DACL access mask `0x00120089` (FILE_GENERIC_READ \| FILE_GENERIC_EXECUTE) |
| RW  | $Volume, $Quota/$Secure, $Extend | 104 | DACL access mask `0x0012009F` (RW + EXECUTE) |
| ROOT | root (".") | 248 | wider DACL with INHERIT_ONLY ACEs that propagate to children |

All three are standard SECURITY_DESCRIPTOR_RELATIVE per MS-DTYP §2.4.6: Revision=1, Control=`0x8004` (SE_DACL_PRESENT | SE_SELF_RELATIVE), Owner=BUILTIN\Administrators (S-1-5-32-544), Group=Administrators, no SACL, self-relative DACL.

**Fix**

`src/mkfs.rs`: bake the three reference SD blobs as `SD_SYSFILE_RO`, `SD_SYSFILE_RW`, `SD_ROOT_DIR` byte constants. Add `sd_for_system_record(rec_num)` selector. `build_system_record` now writes the SD attribute (type 0x50) between $FILE_NAME (0x30) and the caller's `extra_attrs` (which start at type 0x60+), preserving the canonical NTFS attribute-type ordering. Attribute id = 2 (sits between $FN id=1 and the rest).

**Result**

iter14 confirmed present on disk (per-record byte parse: rec 0-4,6-11 carry 104-byte SD; rec 5 carries 248-byte SD). chkdsk verdict on basic-256mib post-iter14: **identical to post-iter13** — `Read-only chkdsk found bad on-disk uppercase table - using system table`, Stage 1 + Stage 2 complete with `64 file records processed` / `68 index entries processed`, then `An unspecified error occurred (frs.cxx 60f)`. The SD addition fixes a real layout divergence (corroborated by byte-diff) but **does not** address the frs.cxx assert. Hypothesis was wrong — root cause lies elsewhere.

Linux baseline tests pass (5 tests: `mkfs::tests::run_encode_decode_roundtrip`, `mkfs::tests::upcase_table_size`, `mkfs_bin_*`, `format_and_parse_back`, `capi_mkfs_then_parse`). `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` clean.

**Next iteration's lead** (recorded for continuity, *not yet attempted*): reference's first 16 MFT records ALL carry FILE magic (records 12-15 are minimal 304-byte placeholders with seq=12..15, flags=0x01 IN_USE, attrs_offset=0x48, bytes_used=0x130). Ours pre-allocates 64 MFT slots but only writes FILE magic into slots 0-11 — slots 12-63 are entirely zero bytes. chkdsk reports "64 file records processed" which suggests it iterates the whole MFT $DATA; the all-zero slots may be triggering the frs.cxx assert.

### iter15: FILE-magic placeholders for unused MFT slots

Session: agent-8a29-2026-05-02. Targeted error: same `frs.cxx 60f` assert as iter14.

**Symptom**

> An unspecified error occurred (6672732e637878 60f).

(Same as iter14 — chkdsk Stage 2 completes "68 index entries processed" then the post-Stage-2 unindexed-file scan trips an internal assert at `frs.cxx:1551`.)

**Diagnostic**

Per-record dump of `reference-mft-16recs.bin` showed every slot 0-15 carries FILE magic. Slots 12-15 specifically carry minimal 304-byte placeholders (`seq=N`, `attrs_offset=0x48`, `bytes_used=0x130`, `flags=0x01` IN_USE — reference treats slots 12-15 as "reserved for future system use", bits 12-15 set in `$MFT:$Bitmap`). Ours pre-allocated 64 MFT slots but only 12 had FILE magic — slots 12-63 were entirely raw zeros. chkdsk reports `64 file records processed` which suggests it iterates the whole `$MFT:$DATA`; raw-zero slots may have been the assert source.

**Per-field diff** *(slots 12..15)*

| Field | reference | ours (pre-iter15) |
|-------|-----------|-------------------|
| FILE magic | present | absent (raw zeros) |
| seq | N (slot number) | n/a (zeros) |
| flags | 0x01 IN_USE | n/a |
| bytes_used | 0x130 (304) | n/a |
| `$MFT:$Bitmap` bit | set | clear |

**Fix**

`src/mkfs.rs`: after writing the 12 system records, loop `slot in 12..mft_records_capacity as u32` and write a FILE-magic placeholder into each unused slot. Placeholder is the **unused** form (FILE magic + seq=0 + IN_USE bit CLEAR + just header + end marker), not reference's IN_USE form, because our `$MFT:$Bitmap` keeps bits 12+ clear (those slots are genuinely free for user files; reference happens to reserve them as system-use). Per the publicly documented NTFS layout, FILE magic with IN_USE=0 is a valid "free MFT slot" representation.

**Result**

iter15 placeholders confirmed on disk (per-record byte parse: slots 12-15 carry `FILE seq=0 flags=0x0 used=80 rec_num=N attrs_off=0x48 end=0xffffffff`). chkdsk verdict on basic-256mib post-iter15: **identical to post-iter14 / post-iter13** — `bad on-disk uppercase table` warning, Stage 1 + Stage 2 complete with same 64/68 counts, then `An unspecified error occurred (frs.cxx 60f)`. Hypothesis was wrong — root cause lies elsewhere again.

Linux baseline tests pass. `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` clean.

**Where the frs.cxx 60f hunt stands after iter11→iter15**

Five evidence-corroborated layout fixes have landed since iter10 — each addressed a real divergence between our output and Microsoft `format.com`'s reference:

| Iter | Fix | Targeted symptom (gone?) | Side-effect on frs.cxx? |
|-----:|-----|--------------------------|-------------------------|
| 11   | `bytes_used` = end_marker_offset + 8 | "First free byte offset corrected" — gone | hidden |
| 12   | `MFT_RECORD_IS_VIEW_INDEX` on $Secure | "Flags for file record segment 9 are incorrect" — gone | revealed |
| 13   | root $I30 indexes all 12 system files | "Detected orphaned file" cascade — gone | persists |
| 14   | $SECURITY_DESCRIPTOR (0x50) on every system record | (none) | persists |
| 15   | FILE-magic placeholders in unused MFT slots | (none) | persists |

The frs.cxx assert has survived every byte-diff-driven layout fix from iter12 onward. **Strong indication the cause is content-level, not layout-level** — most likely candidates that have **not** been investigated:
- `$LogFile` content (we fill with 0xFF / no RSTR; format.com initialises with proper LogFile records)
- `$UpCase` content (our generator emits a non-canonical mapping; chkdsk's standing "bad on-disk uppercase table" warning is the audible symptom, but the assert may also stem from this)
- `$MFT:$Bitmap` non-resident value (worth a byte-diff vs reference)
- `$AttrDef` blob — we emit a hand-rolled 2560-byte canonical NTFS 3.1 table; reference's may differ in subtle ways
- $Volume's `$VOLUME_INFORMATION` flags (we set clean=0; not corroborated)

For the next iteration: capture reference's `$UpCase` clusters off the VM (the existing pipeline doesn't dump them — needs a small `run-windows-test.ps1` patch to copy `clusters[upcase_lcn..upcase_lcn+upcase_clusters]` into `diag/`). Compare byte-for-byte with our `upcase::generate_upcase_table()` output. Bake the canonical bytes in as a const if they differ. That fixes both "bad on-disk uppercase table" and is the most likely candidate for frs.cxx (chkdsk uses upcase for filename collation across the orphan-recovery scan; mismatched table → confused comparison → assert).

### iter16: canonical NT 3.x $UpCase table baked in (replaces char::to_uppercase generator)

Session: agent-8a29-2026-05-02. Targeted: `Read-only chkdsk found bad on-disk uppercase table - using system table` (warning, fires before Stage 1) AND the trailing `frs.cxx 60f` assert (hypothesis: chkdsk uses upcase for filename collation in the orphan-recovery scan; mismatched table → confused compare → assert).

**Symptom**

> Read-only chkdsk found bad on-disk uppercase table - using system table.

(First line of chkdsk output on every run since iter12. Non-fatal — chkdsk falls back to its built-in table — but blocks chkdsk exit 0.)

**Diagnostic — extracting the canonical bytes**

The reference `format.com`-formatted volume's `$UpCase` cluster content is the source of truth (Microsoft's own output, no GPL involvement). Existing pipeline didn't dump it; extracted directly from the VM with this recipe:

1. SSH to VM, mount `reference.vhdx`, assign drive letter to the Basic partition.
2. `fsutil file queryextents F:\$UpCase` → `VCN: 0x0 Clusters: 0x20 LCN: 0x6` (32 clusters at LCN 6, cluster_size = 4096 → 32 × 4096 = 131072 bytes = 128 KiB).
3. Open `\\.\F:` as raw `System.IO.File`, seek to `lcn × cluster_size = 24576`, read 131072 bytes.
4. SHA256: `41c26bc7a12bdaeb26025c93118697c7e3ef81ee048b00fe5cce2a472e0e0742`.
5. `scp` back to Mac, `cp` into `src/upcase-canonical.bin`.

**Per-field diff** (our generator output vs reference, before iter16):

| Code point | char::to_uppercase() | NT canonical | Notes |
|------------|----------------------|--------------|-------|
| U+00B5 (MICRO SIGN) | 0x039C (GREEK CAPITAL MU) | 0x00B5 (unchanged) | NTFS preserves |
| U+00DF (LATIN SMALL SHARP S "ß") | 0x0053 (S) | 0x00DF (unchanged) | NTFS doesn't case-fold ß |
| U+0131 (LATIN SMALL DOTLESS I) | 0x0049 (I) | 0x0131 (unchanged) | NTFS preserves |
| U+0149 (LATIN SMALL N PRECEDED BY APOSTROPHE) | 0x02BC | 0x0149 | NTFS preserves |
| U+017F (LATIN SMALL LONG S) | 0x0053 (S) | 0x017F | NTFS preserves |
| ... | | | |

**327 BMP code points differ in total** between modern Unicode case folding and Microsoft's NT 3.x canonical table. Pattern: NT table is far less aggressive — most characters that Unicode now case-folds, NTFS preserves unchanged.

**Fix**

`src/upcase-canonical.bin`: 131072-byte binary dropped into `src/`, byte-for-byte equal to format.com's reference output (SHA256 above). `src/upcase.rs`: replace the runtime generator with `const CANONICAL_UPCASE: &[u8; 131072] = include_bytes!("upcase-canonical.bin");` and have `generate_upcase_table()` return `CANONICAL_UPCASE.to_vec()`. Cargo's `include_bytes!` adds the `.bin` as a build dependency, so future edits trigger rebuild automatically.

Verified post-build that the resulting `nfs.img`'s `$UpCase` cluster content (LCN read via boot-sector parse + MFT rec 10 $DATA mapping pair decode) hashes to the canonical SHA. U+00B5 → 0x00B5 (was 0x039C with the old generator).

**Result**

`$UpCase` is now byte-for-byte identical to reference. Despite this, **chkdsk still prints `Read-only chkdsk found bad on-disk uppercase table - using system table`**. Implication: chkdsk's "bad upcase" check is keying on something other than the table bytes themselves — possibly the `$UpCase` MFT record's `$STANDARD_INFORMATION` size (ref carries the 48-byte NTFS 1.x form; ours emits the 72-byte NTFS 3.x form on every system record), or some attribute we don't yet write. Frs.cxx 60f assert also unchanged.

iter16 is still a valid fix — the table mismatch was real and the bytes ARE now correct. But the "bad upcase table" message is misleading: it does NOT necessarily indicate table-content corruption.

Linux baseline tests pass. `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` clean.

**Where the hunt stands after iter16**

The remaining unique systemic divergence we've identified between ours and reference, **not yet attempted as a fix**, is:

- **$STANDARD_INFORMATION size on every system record** — ref uses 48 bytes (NTFS 1.x form: just CreationTime + 4×timestamp + DOSAttrs); ours uses 72 bytes (NTFS 3.x form: same fields plus zero MaxVersions/VersionNumber/ClassId/OwnerId/SecurityId/QuotaCharged/USN). chkdsk may demand the 48-byte form on system files. This is the single remaining systematic divergence visible in the byte-diff.

If iter17 ($STD_INFO → 48-byte on system records) doesn't fix the chkdsk warning + frs.cxx, the next layer is content-level checks chkdsk does that aren't visible in the per-record dumps — at which point progress requires either Microsoft's chkdsk source or a much heavier instrumentation pass (capture every disk read chkdsk does, correlate with what we wrote vs what reference wrote).
