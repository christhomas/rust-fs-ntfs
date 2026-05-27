# NTFS Specification

Consolidated, attributed NTFS reference for the `rust-fs-ntfs` library. Synthesised
in our own voice from multiple permissively licensed public references and our own
black-box experimental observations. The spec is not a paraphrase of any single
upstream document — every external source is one input among many and is referenced
by URL only, never mirrored into this tree. Every factual claim is either backed
by an authoritative source or marked `[UNVERIFIED]` for follow-up.

## Layout

```
docs/spec/
├── readme.md                       (this file)
├── ntfs-specification.md           (master TOC + conventions, links to sections/)
├── sections/
│   ├── 01-geometry-boot.md
│   ├── 02-mft-records.md
│   ├── 03-data-runs-bitmap.md
│   ├── 04-indexes-directories.md
│   ├── 05-logfile-journal.md
│   └── 06-special-streams.md
└── notes/                          (working notes, deviation logs, open questions)
```

## Sources we draw on

| Source                                               | Cite as            |
| ---------------------------------------------------- | ------------------ |
| `[MS-NTFS]` Microsoft Open Specification             | `[MS-NTFS §X.Y]`   |
| `[MS-FSCC]` Microsoft Open Specification             | `[MS-FSCC §X.Y]`   |
| Microsoft Learn / MSDN public articles               | `[MSDN: title]`    |
| `ntfs.com` documentation                             | `[NTFSCOM: title]` |
| Academic papers                                      | `[PAPER: ...]`     |
| Our own test output, diagnostics, disk dumps         | `[OBSERVED: ...]`  |

Every factual claim in the spec must trace to one of these. A claim that no
permitted source corroborates is `[UNVERIFIED]` and gets a follow-up entry in
[notes/open-questions.md](notes/open-questions.md). We do not include lead-only
citations to non-authoritative sources.

## Black-box observation

Running closed-source tools and recording what they do to disk bytes is a primary
source for this spec:

- `chkdsk` round-trip — format/mount/use on Windows, dump the raw device, diff
  against the pre-state.
- Windows `format` and `chkdsk /F` — record the bytes each tool writes.
- Mount-and-write smoke — produce a volume with our writer, mount on Windows,
  perform operations, re-read and diff.
- Diff against reference images built by Microsoft tools.

These observations are cited as `[OBSERVED: <test-name or diag-file>]`.

## Attribution tags

| Tag                       | Meaning                                                                |
| ------------------------- | ---------------------------------------------------------------------- |
| `[MS-NTFS §X.Y]`          | Microsoft Open Specification, section X.Y                              |
| `[MS-FSCC §X.Y]`          | Microsoft File System Control Codes spec                               |
| `[MSDN: title]`           | Microsoft Learn / MSDN article (link in section References)            |
| `[NTFSCOM: title]`        | ntfs.com page (link in section References)                             |
| `[PAPER: author-year]`    | Academic paper (full citation in section References)                   |
| `[OBSERVED: test-id]`     | Confirmed by an `rust-fs-ntfs` test, diagnostic, or disk dump          |
| `[CORROBORATED: A,B,...]` | Same fact stated by sources A and B                                    |
| `[UNVERIFIED]`            | Asserted but not yet corroborated by any authoritative source          |
| `[DEVIATES: test-id]`     | Our observation contradicts a written source — see test for details    |

Preference order:

1. **One or more authoritative sources confirm the fact** — cite the most direct one,
   or `[CORROBORATED: A, B]` when more than one source is worth naming.
2. **No authoritative source confirms it yet** — `[UNVERIFIED]`, plus a follow-up
   entry in `notes/open-questions.md` describing the test or research that would
   resolve it.
3. **No basis at all** — don't include the fact.

## Workflow for adding facts

1. Read across the relevant inputs (the listed sources, the `rust-fs-ntfs` source
   tree, and any reference disk dumps we have).
2. For each non-trivial claim, look for corroboration in at least one other source.
3. Write the fact into the section file in your own words, with the appropriate
   attribution tag(s).
4. If the fact is implementation-relevant and untested, add an entry to
   `notes/open-questions.md` describing what test would confirm or refute it.

Do not paste prose, tables, or pseudocode from any external source. Express each
fact as we would write it from scratch.
