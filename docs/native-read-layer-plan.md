# Native Read Layer — removing the `ntfs` crate from the production path

**Status:** proposal / phase 1 in progress
**Author:** instance 3 (workgroup `ntfs`)
**Date:** 2026-06-01

## Why

Today the read side of the library is built on the upstream `ntfs` crate
(`ntfs = "0.4"`, MIT/Apache — *not* a licensing concern). It plays **two
distinct roles**, and only one of them is worth keeping:

1. **Production reader/navigator** — used in 6 `src/` files (`lib.rs`,
   `facade.rs`, `fsck.rs`, `upcase.rs`, and `write.rs` for path
   resolution; `block_io.rs` provides the `Read+Seek` adapter). **This is
   the problem.** Its `NtfsFile` / `NtfsAttributeValue` / `NtfsReadSeek`
   types, `'n/'f` lifetimes, `Read+Seek` bound, and `NtfsError` shape our
   public API and cap what we can build.
2. **Independent verification oracle** — 45 `tests/` files read our writes
   back through it. **This is valuable and stays.** An independent parser
   agreeing with our writer is a strong correctness signal (second only to
   chkdsk).

The plan: **build our own native read layer for the production path, and
demote the `ntfs` crate to a `dev-dependency` used only as a test oracle.**
We keep the verification benefit and shed the API ceiling.

### Concrete constraints the crate imposes today

- **No decompression** — C4 (LZNT1) cannot be wired through it; the codec
  (PR #60) has to read raw clusters itself.
- **No `$ATTRIBUTE_LIST`** awareness in our usage, no case-sensitivity
  (C5), no WOF — every read-side feature fights the crate.
- **Lifetime-pinned raw-pointer hack** in `lib.rs` (`LazyDirState` stores
  `*mut NtfsFile` / `*mut Ntfs` to escape the crate's borrow model) — a
  smell forced purely by interop.
- **Two parsers, one filesystem** — our write path and the upstream read
  path are separate models; the `facade` bolts them together. Divergence
  is a latent bug class.

## What we already own (audit, 2026-06-01)

~80–85% of the read path is **already native** (built for the write side):

| Capability | Module | Status |
|---|---|---|
| Boot/BPB geometry (cluster size, MFT LCN, record size) | `mft_io.rs` | ✅ native |
| Read MFT record by number + USA fixup | `mft_io.rs` | ✅ native |
| Attribute iteration / `find_attribute` / resident+non-resident headers | `attr_io.rs` | ✅ native |
| Runlist decode, `vcn_to_lcn`, hole detection | `data_runs.rs` | ✅ native |
| `$INDEX_ROOT` lookup, INDX block read+fixup, `$Bitmap:$I30` | `index_io.rs` / `idx_block.rs` | ✅ native |
| File-reference encode, name namespace, NT time now | `record_build.rs` | ✅ native |
| `$UpCase` canonical bytes + COLLATION_FILE_NAME | `upcase.rs` | ✅ native (table); load via upstream |
| `BlockIo` positioned I/O | `block_io.rs` | ✅ native |

## Gaps to close (the 15–20%)

1. **Path resolution** (`/a/b/c` → record number) — currently 100%
   upstream (`write.rs::resolve_path_to_record_number_io`). Pure assembly
   of primitives we already have. **← phase 1.**
2. **Non-resident value reader** — a `read(offset, len)` / streaming view
   over `$DATA` (and any attribute) that walks runs, reads clusters via
   `BlockIo`, and zero-fills holes. We have `vcn_to_lcn` + `BlockIo`.
3. **Directory enumeration** — iterate *all* entries across `$INDEX_ROOT`
   + INDX blocks, skipping the DOS namespace. We have the scanners.
4. **`$UpCase` load from volume** — read MFT record 10 natively (trivial
   given native MFT read).
5. **`$ATTRIBUTE_LIST`** — chase attributes that overflow into extension
   records. **A genuine new gap** (write side doesn't handle it either).
6. **Structured-value decode** — `$STANDARD_INFORMATION` / `$FILE_NAME`
   timestamps + flags, `$VOLUME_INFORMATION`; `NtfsTime`↔Unix (arithmetic,
   epoch offset 11_644_473_600 s).

## Target architecture (the ambitious part)

A cohesive model built directly on `BlockIo`, shared by read **and** write
(one source of truth), with our own error type:

```
BlockIo  ──►  Volume (boot geometry, $UpCase, $MFT location)
                │
                ├─ Inode (one MFT record: flags, link count, attrs;
                │         transparently follows $ATTRIBUTE_LIST)
                │     ├─ attributes() / find(type, name)
                │     └─ AttrReader (resident | non-resident runs |
                │                    sparse holes | LZNT1 | WOF)  ← seekable
                │
                └─ Dir (directory inode)
                      ├─ lookup(name)  (collation-aware; case-sensitive opt)
                      └─ entries()     (merged $INDEX_ROOT + $INDEX_ALLOCATION)
```

Design principles:
- **`BlockIo`-native** — no `Read+Seek` cursor adapter (`IoReadSeek`),
  no raw-pointer lifetime escapes. Positioned I/O end to end.
- **One model for read + write** — `Inode`/`AttrReader` are the same
  primitives the writers mutate; no second parser to drift.
- **Our error type** — `enum NtfsReadError` (or shared `FsError`), not the
  upstream `NtfsError`.
- **Features first-class** — compression (C4), case-sensitivity (C5),
  `$ATTRIBUTE_LIST`, WOF become natural extension points, not fights.
- **Streaming + zero-copy where it pays** — `AttrReader` reads ranges
  without materialising whole attributes (the upcase/data paths today
  read-loop into a Vec).

## Phased migration (each phase independently shippable + cross-checked)

Every phase keeps the upstream crate as the **test oracle**: new native
code is validated by reading the same volume both ways and asserting
equality, plus existing chkdsk/matrix gates for anything that round-trips
through write.

- **Phase 1 — native path resolver** *(this branch)*. New additive module
  `src/read/` (no existing call site touched). `resolve_path(io, path) ->
  record_number` via `mft_io` + `index_io` + `idx_block`. Test: cross-check
  against `ntfs` crate resolution over a self-generated volume. Zero
  collision.
- **Phase 2 — native attribute reader.** `AttrReader` over runs + holes;
  cross-check byte-for-byte against `data_attr.value().read()` for resident,
  non-resident, and sparse files. Wire `$UpCase` load off upstream.
- **Phase 3 — native directory enumeration + structured values.** `Dir::entries()`,
  `$STANDARD_INFORMATION`/`$FILE_NAME` decode, `NtfsTime`. Cross-check the
  `facade` listing + stat output.
- **Phase 4 — `$ATTRIBUTE_LIST`.** Extension-record chasing (new capability).
- **Phase 5 — migrate call sites.** Repoint `lib.rs`, `facade.rs`,
  `fsck.rs`, `upcase.rs`, `write.rs::resolve_*` onto the native layer, one
  file per PR, each diffed against upstream behaviour.
- **Phase 6 — demote the crate.** Move `ntfs` from `[dependencies]` to
  `[dev-dependencies]`; it remains the oracle in `tests/`. Delete
  `IoReadSeek` and the `LazyDirState` raw-pointer hack.

## Non-goals / guardrails

- **Do not** drop the crate from `tests/` — that's the oracle; losing it
  removes our independent read-back check.
- **Do not** big-bang. Each phase is additive + cross-checked before any
  call-site flips.
- **Coordinate** phase 5 with whoever owns `write.rs` — the path resolver
  lives there and the write path also consumes the crate.

## Open questions for the team

- Error model: one shared `FsError` for read+write, or a read-specific
  type that the facade maps?
- Do we want a borrowed (`&[u8]` into a cached record) `Inode` or an owned
  one? (Borrowed is faster but reintroduces lifetime ergonomics; owned is
  simpler for the FFI boundary.)
- Naming: `src/read/` module tree vs a flat `src/reader.rs`.
