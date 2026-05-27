# Implementation plan — `$Secure` + `$Extend` for `chkdsk /scan` exit 13

## Status update (2026-05-27)

**S1–S4 shipped and sealed.** This document was written on 2026-05-21 as a forward-looking plan. By 2026-05-27 the following sub-PRs landed in `main` (PR #52, branch `staging-4`):

| Sub-PR | Status | Notes |
|:------:|:------:|-------|
| **S1** | ✅ shipped | Empty `$SDS` stream + skeleton `$SDH`/`$SII` index roots |
| **S2** | ✅ shipped | Canonical SD entry at security_id=0x100; all system files assigned that ID |
| **S3** | ✅ shipped | `$Extend` directory at rec 11 with populated `$I30` |
| **S4** | ✅ shipped | `$Extend\$Reparse` (rec 17) with empty `$R` index; VIEW_INDEX flag set |
| **S5** | ⏭ skipped | `$RmMetadata\$TxfLog\$Tops:$T` — chkdsk /scan exits **0** without it once S1–S4 are present |

`chkdsk /scan` no longer exits 13 after these changes. All 42 test matrix scenarios pass with `status: ok` (sealed at commit `dcdb46d`, 2026-05-27T03:18:48Z).

The implementation also uses MFT records **16/17/18** for `$ObjId/$Reparse/$Quota` (not the conventional 24/25/26 from third-party docs) — Windows accepts either numbering.

The remaining sections of this document are preserved as historical record of the investigation and as reference material for future `$Secure` runtime work (per-file ACL assignment, SD deduplication).

---

## Why this doc exists

[`docs/future-features.md` §3.1](./future-features.md) documented the
`chkdsk /scan` exit-13 ceiling: volumes that pass `chkdsk` read-only +
`chkdsk /F` (offline) clean, but `chkdsk /scan` (online) consistently
exits 13 with "errors queued for offline repair." A multi-month
investigation had ruled out 11 plausible structural hypotheses
([`docs/chkdsk-improvement-findings.md`](./chkdsk-improvement-findings.md)
§3), and the remaining hypothesis space was "deep-Windows-internals."

The Iter H Procmon trace (2026-05-21, recorded in
[`chkdsk-improvement-findings.md`](./chkdsk-improvement-findings.md)
§6.9) replaced that hypothesis space with **evidence**: three specific
NTFS files chkdsk opens on our volume that we don't ship.

| File chkdsk opens                                | Provides | Our state |
|--------------------------------------------------|----------|-----------|
| `$Secure:$SDS`                                   | Security-descriptor data stream | empty stub |
| `$Extend\$Reparse:$R:$INDEX_ALLOCATION`          | Reparse-points view-index       | `$Extend` (rec 11) unwritten |
| `$Extend\$RmMetadata\$TxfLog\$Tops:$T`           | TxF transactional log           | not implemented |

This document plans the implementation arc for those three structures.

## Scope and non-goals

**In scope**: enough on-disk content that `chkdsk /scan` finds each
file when it opens them and processes their contents without
triggering the corruption flag. Specifically:

- A minimal but well-formed `$Secure:$SDS` data stream with at least
  one canonical security-descriptor entry.
- `$Secure:$SDH` + `$Secure:$SII` view-index attributes referencing
  that entry.
- A `$Extend` directory (rec 11) with `$Reparse` and
  `$RmMetadata\$TxfLog\$Tops` as sub-files containing the minimum
  attribute set chkdsk requires.

**Not in scope** (deferred):

- Runtime SD allocation (per-file ACLs the caller can set). All
  fresh-formatted files keep the default SD; `set_file_security` /
  callable SD APIs are a separate body of work.
- Active TxF transaction support. The `$Tops:$T` log will exist as
  the canonical "empty log" sentinel, not as a real transactional
  store.
- Reparse-point indexing for user files. The `$Reparse` index will
  be present and empty.

## Sequencing — small PRs with trace iterations between

The corroborated-debug discipline calls for the smallest possible
change backed by evidence, then a re-measure. We follow that here.
Each
sub-PR is independently mergeable; after each one lands, re-run
[`scripts/procmon-chkdsk-trace.ps1`](../scripts/procmon-chkdsk-trace.ps1)
and append the iteration result to
[`chkdsk-improvement-findings.md`](./chkdsk-improvement-findings.md)
§6.9.

| Sub-PR | Scope                                                                                                       | Trace target                                                                       |
|:------:|-------------------------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------|
| **S1** | Add empty `$SDS` named-`$DATA` stream to rec 9, plus skeleton `$SDH`/`$SII` index roots (empty, no entries) | Does `$Secure:$SDS` open succeed? Does chkdsk enumerate streams without complaint? |
| **S2** | Populate `$SDS` with one canonical SD entry; add matching `$SDH`/`$SII` entries; assign that ID to system files' `$STANDARD_INFORMATION.security_id` | Does `chkdsk /scan` proceed past the SD probe? |
| **S3** | Build `$Extend` directory at rec 11 with empty `$I30`                                                       | Does the `$Extend` open succeed?                                                   |
| **S4** | Add `$Extend\$Reparse` file with empty `$R` named index                                                     | Does the reparse probe succeed?                                                    |
| **S5** | Add `$Extend\$RmMetadata` directory + `$TxfLog` sub-directory + `$Tops` file with `$T` named stream         | Does `chkdsk /scan` exit 0?                                                        |

If any sub-PR alone shifts the verdict from 13 to 0, the remaining
sub-PRs become hygiene (`chkdsk` doesn't need them today, but a
future Windows release might).

## Reference material

All citations must be from publicly published sources per the
findings doc's §1.6 ("public-spec-only citation rule"). No
reverse-engineered GPL'd implementations. Sources used below:

- **MS-FSCC** ([learn.microsoft.com/openspecs/windows_protocols/ms-fscc/](https://learn.microsoft.com/openspecs/windows_protocols/ms-fscc/))
  — NTFS attribute layouts, system-file purposes.
- **MS-DTYP** — `SECURITY_DESCRIPTOR` and `ACL` binary formats.
- **Windows Internals, 7th ed.** — `$Secure` caching mechanism,
  `$SDH` / `$SII` index semantics, TxF overview.
- **Byte-diff against Microsoft `format.com` output** — gathered
  via the existing CI step "Build a reference Microsoft-formatted
  NTFS volume + diff against ours" (see
  [`.github/workflows/ci.yml`](../.github/workflows/ci.yml)). For
  fields the spec leaves implementation-defined (e.g. the SDH hash
  algorithm), the reference dump is the ground truth.

## Sub-PR S1: empty `$SDS` stream + empty `$SDH`/`$SII` skeletons

### Why an empty version first

Iter H's trace shows chkdsk opens `\Device\…\$Secure:$SDS` and the
open fails with `STATUS_OBJECT_PATH_NOT_FOUND`. The first hypothesis
to test is whether a successful open of an empty stream is enough to
clear the failure mode chkdsk reports — independent of the stream's
contents.

### Code-touch surface

[`src/mkfs.rs`](../src/mkfs.rs):

- Rec 9 builder (currently at lines ~777–807). Add three
  attributes alongside the existing `$DATA` empty stub:
  - Named `$DATA` stream "$SDS" (resident, value-length 0).
  - Named `$INDEX_ROOT` "$SDH" (resident, empty index header).
  - Named `$INDEX_ROOT` "$SII" (resident, empty index header).
- The existing rec 9 `$DATA` stub remains (it's the unnamed default
  stream).

### Spec citations

- MS-FSCC §2.4 attribute headers (resident attribute layout, name
  offset/length).
- MS-FSCC §2.4.9 `$INDEX_ROOT` structure.

### Effort estimate

~80 LOC: one named-stream builder reuse + two named-index-root
builders. The `build_resident_unnamed` helper already exists at
[`src/mkfs.rs:1443`](../src/mkfs.rs#L1443); add a
`build_resident_named` companion that accepts a UTF-16 name.

### Acceptance

Re-run the trace harness. The `\Device\…\$Secure:$SDS` open should
succeed (visible in the FileIo Create events). If `chkdsk /scan`
exit stays at 13, proceed to S2.

## Sub-PR S2: one populated SD entry + matching index entries

### Format of `$SDS`

Per MS-FSCC §2.4 (`$Secure` system file) and Windows Internals:

```text
SDS_ENTRY {
    HASH         : u32  -- hash of the SD blob below
    SECURITY_ID  : u32  -- monotonically increasing, starts at 0x100
    SDS_OFFSET   : u64  -- byte offset of this entry within $SDS
    SDS_SIZE     : u32  -- total entry size including this header
    SD           : SECURITY_DESCRIPTOR  -- MS-DTYP §2.4.6
}
-- followed by 0-pad to 16-byte boundary
-- the entire entry is also written to offset+0x40000 (mirror)
```

The SECURITY_ID space starts at 0x100; lower values are reserved.
Microsoft's convention is to assign 0x100, 0x101, 0x102… in
allocation order.

### `$SDH` (Security Descriptor Hash) view-index

Keyed on `(hash, security_id)`, collation `COLLATION_NTOFS_SECURITY_HASH`
(per MS-FSCC §2.4 — but verify via reference dump; this is one of
the fields the spec under-specifies). Each entry maps a SD payload
hash to its $SDS offset.

Key: `{ hash: u32, security_id: u32 }` (8 bytes).
Value: `{ hash: u32, security_id: u32, sds_offset: u64, sds_size: u32 }`
(20 bytes).

### `$SII` (Security ID Index) view-index

Keyed on `security_id` (4 bytes). Collation
`COLLATION_NTOFS_ULONG`.

Key: `security_id: u32` (4 bytes).
Value: `{ hash: u32, security_id: u32, sds_offset: u64, sds_size: u32 }`
(20 bytes — same value as $SDH).

### Canonical SD to ship

The minimum that satisfies chkdsk is one SD that all system files
can reference. Microsoft uses approximately:

- Owner: BUILTIN\Administrators (SID S-1-5-32-544)
- Group: BUILTIN\Administrators
- DACL: 2 ACEs
  - Allow BUILTIN\Administrators FILE_ALL_ACCESS
  - Allow LocalSystem (S-1-5-18) FILE_ALL_ACCESS
- SACL: none

This is the same SD shape the codebase already ships as `SD_SYSFILE_RW`
const at [`src/mkfs.rs:163`](../src/mkfs.rs#L163) (used for the
inline `$SECURITY_DESCRIPTOR` attribute on rec 3 + 9). Reuse it
directly as the $SDS payload.

### Hash algorithm

Per Windows Internals, the SD hash is a 32-bit value derived from
the SD bytes. The exact algorithm is not publicly specified, but
multiple sources (Windows Internals 7e, sysadmin docs) describe it
as a rolling sum-and-shift, NOT MD4 as previously hypothesised. We
verify by dumping reference's $SDS entries and back-computing.

**Method**: format a 256 MiB reference volume with `format.com`,
dump $SDS via the existing CI reference step, extract the
{hash, sd_bytes} pairs, brute-force the algorithm by trying a
shortlist of candidates (Pearson, FNV-1a, simple rolling sum).
The §1.6 rule allows observational citation when the spec is
silent: "we observed reference computes H(sd) = X" suffices.

If the hash can't be matched from reference, fall back to: ship
hash=0 (which chkdsk should accept since hash-lookup is a perf
optimisation, not a correctness requirement — `$SDH` collisions
are linear-search-resolved via `$SII`).

### Updating `$STANDARD_INFORMATION.security_id`

Every system MFT record's `$STD_INFO` currently has security_id=0
(default). With one SD shipped at id=0x100, update every system
record's `$STD_INFO.security_id` to point at it.

This is a 1-line change in
[`src/mkfs.rs`](../src/mkfs.rs) `build_std_information`'s value-
construction block.

### Code-touch surface

- New module `src/sds.rs` (~150 LOC): SDS entry serialization, SDH
  and SII value packing, hash function.
- [`src/mkfs.rs`](../src/mkfs.rs) rec 9 builder: replace S1's empty
  stubs with populated versions; allocate a cluster for $SDS data
  stream; encode run-list; place data in cluster.
- [`src/mkfs.rs`](../src/mkfs.rs) `build_std_information`: parameter
  for security_id (default 0x100 for system records).

### Effort estimate

~250 LOC for the full path (S1+S2 combined ~330). Plus hash-
algorithm investigation: 1–3 hours of reference-dump analysis.

### Acceptance

Re-run trace. Look for: `$Secure:$SDS` reads succeed, chkdsk doesn't
report SD-cache complaints. If `/scan` still 13, proceed to S3+.

## Sub-PR S3: `$Extend` directory shell

### Why rec 11

Rec 11 is `$Extend` per NTFS convention (slot fixed by
`KnownNtfsFileRecordNumber::Extend`). Today we leave it unwritten
(§2.8) — that was an iter18a decision after agent-8934 tested both
"empty directory" and "flat file" forms and concluded neither helped
`frs.cxx 60f`.

The Iter H trace tells us the question wasn't whether rec 11 helps
`frs.cxx 60f` — it was whether chkdsk needs `$Extend` to **exist
as a directory** so it can recursively open `$Extend\$Reparse` and
`$Extend\$RmMetadata\…`. Today the open of those paths fails at
the `$Extend` level.

### Code-touch surface

- [`src/mkfs.rs`](../src/mkfs.rs) — un-skip rec 11; build it as a
  small directory with an empty `$I30` and place it in `mft_buf`.
- Root `$I30` does NOT need an entry for `$Extend`; reference's
  rec 11 has its own $FILE_NAME with parent=5 (root) but `$Extend`
  is a hidden file and not enumerated.

### Effort estimate

~50 LOC. Reuses the `build_directory_record` path used for root.

### Acceptance

`$Extend` open succeeds. Trace shows chkdsk recursing into it.
If `/scan` still 13, proceed to S4.

## Sub-PR S4: `$Extend\$Reparse` with empty `$R` index

### Layout

`$Extend\$Reparse` is a file with:
- `$STD_INFO`
- `$FILE_NAME` (parent = rec 11, name "$Reparse")
- Named index `$R`:
  - `$INDEX_ROOT` "$R" (resident, header only — no entries)

The `$R` index in user-level volumes would list every reparse
point on disk. On a fresh format, it's empty.

### Allocation

Need a new MFT slot — chkdsk in our trace opens it via a NTFS
path resolution that does `$Extend` + lookup "$Reparse" via the
directory's $I30. So `$Extend`'s $I30 needs an entry for the
new record. Allocate slot 16 (first slot after the
canonical 0–15 reserved range).

### Effort estimate

~80 LOC. Reuses index-entry-build code.

### Acceptance

`$Extend\$Reparse:$R:$INDEX_ALLOCATION` open succeeds.
If `/scan` still 13, proceed to S5.

## Sub-PR S5: `$Extend\$RmMetadata\$TxfLog\$Tops:$T`

### Layout

Nested:
- `$Extend\$RmMetadata` — directory, MFT slot 17.
- `$Extend\$RmMetadata\$TxfLog` — directory, MFT slot 18.
- `$Extend\$RmMetadata\$TxfLog\$Tops` — file, MFT slot 19, with:
  - `$STD_INFO`
  - `$FILE_NAME` (parent = slot 18)
  - Named `$DATA` "$T" (non-resident, points at a cluster
    of empty / sentinel TxF log content)

Each parent directory needs a $I30 entry for the next level down.

### TxF log sentinel content

TxF's `$Tops:$T` stream is the "transaction order pages" log.
Per Windows Internals, the format is:
- A series of pages with header `TOPS` magic
- "No transactions" sentinel: all zero or a single empty page

A 4 KiB single zero-filled page should satisfy "no transactions"
without violating spec; chkdsk's check is presence + magic-byte
sniff, not full log parse.

### Allocation

3 new MFT slots (17, 18, 19) + 1 cluster for $T data.

### Effort estimate

~120 LOC for the three records + cluster placement + $I30 entries.

### Acceptance

`$Extend\$RmMetadata\$TxfLog\$Tops:$T` open + read succeeds.
`/scan` exit MUST drop to 0 here, otherwise we have a 12th
disproven hypothesis and a new investigation.

## Risk register

| # | Risk                                                                                                  | Mitigation                                                                                    |
|---|-------------------------------------------------------------------------------------------------------|-----------------------------------------------------------------------------------------------|
| 1 | The hash algorithm for $SDH is wrong, chkdsk fails the SD cache check.                                | S2 has a hash-= 0 fallback; SII is the authoritative lookup, SDH is a perf cache only.        |
| 2 | Allocating MFT slots 16–19 breaks something that assumed only slots 0–15 are populated.               | Audit `mft_records_capacity` and `make_mft_internal_bitmap` callers; add bits for new slots.  |
| 3 | The `$Extend` directory layout in chkdsk's check is stricter than what we ship (specific attrs, flags). | Iter trace surfaces this — re-run after each sub-PR.                                          |
| 4 | TxF log format has a stricter "no transactions" sentinel than 4 KiB of zeros.                         | Byte-diff our $Tops:$T against reference's; copy the canonical empty form.                    |
| 5 | All five sub-PRs land and `/scan` still exits 13.                                                     | We've eliminated one large hypothesis space and learned what's NOT the cause — next iteration starts from a tighter Procmon trace. |

## Testing strategy

- The existing `format_and_parse_back` integration test
  ([`tests/mkfs_roundtrip.rs`](../tests/mkfs_roundtrip.rs)) must
  continue to pass — every sub-PR adds fields that the test should
  optionally assert on (volume re-parses, system files listed,
  $Secure stream present).
- After each sub-PR, run `scripts/procmon-chkdsk-trace.ps1` on the
  VM and record results in `chkdsk-improvement-findings.md` §6.9
  as a sub-iteration (H1, H2, …) with the file-by-file before/
  after table.
- The mkfs unit tests in [`src/mkfs.rs`](../src/mkfs.rs) (mod
  `tests`) should pick up new helpers as soon as they exist.

## Out-of-band: what if /scan still 13 after all of S1–S5

The remaining hypothesis space at that point is significantly
narrower than today. Productive next moves:

1. **Re-trace with stricter filter**. Look for non-Create
   `OperationEnd` events with non-zero NTSTATUS on our volume —
   that's chkdsk getting an error inside a successful open.
2. **Compare reference's $Tops:$T bytes** against ours. If TxF
   log validation is stricter than presence, byte-diff against
   format.com.
3. **Add ETW providers** — `Microsoft-Windows-NTFS` event provider
   (GUID `{3FF37A1C-A68D-4D6E-8C9B-F79E8B16C482}`) exposes the
   driver's own opinion of corruption. Captures the kernel's
   "why I think this is corrupt" before chkdsk even runs.
