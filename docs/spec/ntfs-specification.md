# NTFS Specification

> Consolidated, attributed reference for the `rust-fs-ntfs` library. See
> [readme.md](readme.md) for licensing posture, attribution scheme, and the list
> of permitted sources.

## Table of contents

| §   | Topic                          | File                                                                       |
| --- | ------------------------------ | -------------------------------------------------------------------------- |
| 1   | Volume geometry & boot sector  | [sections/01-geometry-boot.md](sections/01-geometry-boot.md)               |
| 2   | MFT & records                  | [sections/02-mft-records.md](sections/02-mft-records.md)                   |
| 3   | Data runs & cluster allocation | [sections/03-data-runs-bitmap.md](sections/03-data-runs-bitmap.md)         |
| 4   | Indexes & directories          | [sections/04-indexes-directories.md](sections/04-indexes-directories.md)   |
| 5   | $LogFile & journal             | [sections/05-logfile-journal.md](sections/05-logfile-journal.md)           |
| 6   | Special streams                | [sections/06-special-streams.md](sections/06-special-streams.md)           |

Supporting docs:

- [readme.md](readme.md) — sources, attribution tags, workflow
- [notes/open-questions.md](notes/open-questions.md) — `[UNVERIFIED]` claims awaiting test
- [notes/references.md](notes/references.md) — master reference list

## Document conventions

- All factual claims carry an attribution tag — see
  [readme.md → Attribution tags](readme.md#attribution-tags).
- "MFT entry" and "MFT record" are interchangeable; prefer "record".
- "Cluster" = NTFS allocation unit; "sector" = underlying device block. Cluster ≥ sector.
- Hex byte offsets are zero-based within the structure unless stated otherwise.
- Little-endian unless explicitly noted.

### Status legend

| Symbol | Meaning                                                                  |
| ------ | ------------------------------------------------------------------------ |
| ✅     | Implemented in `rust-fs-ntfs` and exercised by tests                     |
| 🟡     | Partial — read path works, write path TBD (or vice versa)                |
| ⛔     | Not implemented — captured here for reference only                       |
| 🔬     | Tracked as `[UNVERIFIED]` — needs an experimental confirmation pass      |

### Page layout for section files

Every file under `sections/` follows the same template:

```markdown
[← Prev: <prev-title>](<prev-file>) | [TOC](../ntfs-specification.md) | [Next: <next-title>](<next-file>)

# <N>. <Section title>

## Overview
…

## <Subsection> {#anchor-slug}
…facts with attribution tags…

## References
…links cited in this section…

## Open questions
…section-local `[UNVERIFIED]` items…

[← Prev: <prev-title>](<prev-file>) | [TOC](../ntfs-specification.md) | [Next: <next-title>](<next-file>)
```

- Every non-trivial subsection has a stable anchor slug `{#anchor-slug}` so other
  sections can deep-link into it as `[§N.M Title](path#anchor-slug)`.
- The first and last section files use only the directions that exist (no Prev on §1,
  no Next on the last).
- Prefer relative paths: `../ntfs-specification.md`, `02-mft-records.md`.
- When citing another section inline, link the anchor:
  `see [USA fixup](02-mft-records.md#usa-fixup) for the per-sector tail rewrite`.

## Cross-section concerns

- **NTFS versions** (1.2 / 3.0 / 3.1) — definitional content lives in §1
  ([Volume geometry](sections/01-geometry-boot.md)); per-version feature differences
  are repeated locally in each section that depends on them.
- **Update Sequence Array (USA)** fixup — applies to every multi-sector record (MFT,
  INDX, RCRD). Defined once in [§2 MFT records](sections/02-mft-records.md#usa-fixup)
  and referenced from §4 and §5.
- **Endianness, signature checks, magic numbers** — listed in
  [§1 Volume geometry](sections/01-geometry-boot.md) alongside the boot sector
  reference.
- **Repair semantics** — `rust-fs-ntfs` does not implement repair. Repair-flavoured
  rules (chkdsk-style validation, `$LogFile` replay) are out of scope; we only
  document the format invariants those rules imply.

## Capability and knowledge gaps

This section is the negative-knowledge complement to the per-section content.
Together, "what we know" (spec sections) + "what we do not know or have not
implemented" (this section) = 100% of the design space.

### Unimplemented features

These are format-level capabilities that `rust-fs-ntfs` does not implement.
Reading volumes that use them may be partial; writing them is not supported.

| Feature | Section | Tracking |
| ------- | ------- | -------- |
| LZNT1 compression (read + write) | §6 LZNT1 | Not started |
| `$LogFile` crash-recovery replay | §5 WAL recovery | Not started |
| DOS 8.3 short-name generation (write) | §4 DOS alias | `status.md` |
| B+ tree split/merge for large directories | §4 `$INDEX_ALLOCATION` | `status.md` |
| `$MFTMirr` maintenance on every write | §2 `$MFTMirr` | Not started |
| `$ATTRIBUTE_LIST` base-overflow emission | §2 `$ATTRIBUTE_LIST` | Not started |
| Backup boot sector bidirectional sync | §1 backup boot | `status.md` |
| `$Extend\$Reparse` index entry maintenance | §6 `$Reparse` index | `status.md` |
| Non-resident `$BITMAP:$I30` (large dirs) | §4 index bitmap | `status.md` |
| Bad-cluster relocation (`$BadClus` updates) | §3 `$BadClus` | Not started |
| EFS (`$EFS` `$LOGGED_UTILITY_STREAM`) | §6 EFS | Not started |
| Transactional NTFS (`$TXF_DATA`) | §6 `$TXF_DATA` | Not started |
| `$Quota` enforcement | §6 `$Quota` | Not started |
| `$ObjId` maintenance on create/rename/delete | §6 `$ObjId` | Not started |

### Highest-priority unverified claims

These are structural claims that affect write-path correctness but have not yet
been confirmed by a black-box test or a permitted spec citation. Items marked
`[BLOCKING]` are suspected to cause chkdsk failures or Windows mount errors if
wrong.

| Claim | Section | Priority | Notes |
| ----- | ------- | -------- | ----- |
| MFT record CRC32 footer is computed *after* USA revert, on post-fixup bytes | §2 USA | `[BLOCKING]` | Not yet validated; `rust-fs-ntfs` does not emit or check it |
| `$MFTMirr` mirrors exactly `N = mirror_size / record_size` records, not a hardcoded 4 | §2 `$MFTMirr` | `[BLOCKING]` | Used in repair decision matrix; not exercised by any test |
| `COLLATION_NTOFS_SID = 0x11` — numeric value for `$Quota:$O` | §4 collation | Medium | Not emitted by codebase; numeric value is conventional |
| `$VOLUME_INFORMATION` flag bit values (`0x0001` dirty, `0x8000` modified_by_chkdsk, etc.) | §6 `$Volume` | Medium | Conventional values; not tested against `[MS-NTFS]` |
| `$Secure:$SDS` entry MUST NOT span a 256 KiB mirror boundary | §6 `$SDS` | Medium | Implied by mirror granularity; not verified against Windows |
| `$STANDARD_INFORMATION` timestamp authority — SI wins over `$FILE_NAME` on conflict | §2 timestamps | Low | Operational observation only; no spec citation |
| `chkdsk /F` NEVER collapses base-overflow extension records back to base | §2 `$ATTRIBUTE_LIST` | Low | Behaviour of closed-source chkdsk is not documented |
| Backup boot sector is exactly at the last sector of the partition | §1 backup boot | Low | Conventional; `rust-fs-ntfs` writes it there but doesn't test recovery from it |

### What the spec does NOT cover

- Kernel-internal memory management, caching, or paging within `ntfs.sys`.
- Windows kernel object model (FCBs, SCBs, CCBs) — format-level only.
- SMB/NFS or network-layer interaction with NTFS.
- Volume Shadow Copy Service (VSS) / snapshot interaction.
- Storage Spaces, BitLocker, or hardware RAID interplay with NTFS geometry.

---

## Changelog

Append-only. Date, change, rationale.

| Date       | Change                                                                       |
| ---------- | ---------------------------------------------------------------------------- |
| 2026-05-03 | Skeleton + first-cut sections drafted across §1–§6                           |
| 2026-05-25 | [UNVERIFIED]→[OBSERVED] upgrades: SDH hash (src/sds.rs), collation codes (src/mkfs.rs), file attribute bits (src/record_build.rs, src/write.rs). Fixed $SII collation code (0x10 not 0x13). Added FA_NTFS_VIEW_INDEX and FILE_ATTRIBUTE_REPARSE_POINT to §2 table. Corrected namespace heuristic in §4. Added capability-gap matrix. |
