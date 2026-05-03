# NTFS Specification

> Consolidated, attributed reference for the `rust-fs-ntfs` library. See
> [README.md](README.md) for licensing posture, attribution scheme, and the list
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

- [README.md](README.md) — sources, attribution tags, workflow
- [notes/open-questions.md](notes/open-questions.md) — `[UNVERIFIED]` claims awaiting test
- [notes/references.md](notes/references.md) — master reference list

## Document conventions

- All factual claims carry an attribution tag — see
  [README.md → Attribution tags](README.md#attribution-tags).
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

## Changelog

Append-only. Date, change, rationale.

| Date       | Change                                                                       |
| ---------- | ---------------------------------------------------------------------------- |
| 2026-05-03 | Skeleton + first-cut sections drafted across §1–§6                           |
