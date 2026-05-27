# fs-ntfs — pure-Rust NTFS driver

A pure-Rust read/write NTFS driver, dual-licensed Apache-2.0 / MIT,
with no kernel dependencies and no FFI to a C-language NTFS library.
The crate ships a stable C ABI (`fs_ntfs_*`) so it can be linked
from C, C++, Go (via cgo), Swift, or any other language with FFI.
Mount + write of freshly-formatted volumes is validated end-to-end
against Microsoft's `chkdsk` running on real Windows VMs — that's
the test contract, not byte-equivalence with any third-party tool.

## Status

The crate is in **active development**, not yet 1.0.

What's solid today:

- **Read** — every NTFS read path that upstream `ntfs = "0.4"`
  (Colin Finck's read-only parser, MIT/Apache-2.0) supports works
  here unchanged: stat, readdir, file content, ADS, reparse points,
  symlinks, junctions, Unicode names.
- **Write** — original work layered on top of the upstream reader:
  resident + non-resident `$DATA` writes, resident → non-resident
  promotion, grow / truncate, create / unlink / mkdir / rmdir,
  rename (same- and variable-length), hard links, ADS write/delete,
  reparse points, EAs, timestamps, file-attribute flag toggling.
- **Recovery** — dirty-flag detect + clear, `$LogFile` reset,
  end-to-end fsck. Both path-based and callback-transport APIs.
- **mkfs** — pure-Rust formatter that produces volumes Microsoft's
  `chkdsk /scan` accepts and Windows `ntfs.sys` mounts and writes
  to. This was the multi-month wall the project broke through on
  **2026-05-02** (see Changelog).

What's still landing:

- B+ tree insert / delete in `$INDEX_ALLOCATION` once a directory
  has overflowed out of `$INDEX_ROOT` (W3.2 / W3.3 in
  `docs/future-features.md`).
- `$MFT` self-growth when `$MFT:$Bitmap` is exhausted (W2.6).
- A handful of mkfs scenarios in the multi-VM matrix that still
  trip `chkdsk` in repair mode — tracked in `test-matrix.json`.

## Features

### Read

| Operation | Status |
|---|---|
| Mount + parse boot sector / `$MFT` / `$Volume` | yes |
| `stat` (resident + non-resident attrs) | yes |
| `readdir` (`$INDEX_ROOT` + `$INDEX_ALLOCATION`) | yes |
| `read` (resident, non-resident, fragmented `$DATA`) | yes |
| `readlink` (symlink + junction reparse points) | yes |
| Alternate Data Streams (named `$DATA`) | yes |
| Extended Attributes (`$EA`, `$EA_INFORMATION`) | yes |
| `$OBJECT_ID` (16-byte GUID) | yes |
| Volume statistics (`$Bitmap`, `$MFT:$Bitmap`) | yes |

### Write

| Operation | Status |
|---|---|
| In-place data write (existing non-resident `$DATA`) | yes |
| Replace contents (resident, with auto-promote on overflow) | yes |
| `grow` / `truncate` non-resident `$DATA` | yes |
| `create_file` / `unlink` | yes (parents that fit in `$INDEX_ROOT`) |
| `mkdir` / `rmdir` | yes (parents that fit in `$INDEX_ROOT`) |
| `rename` (same-length + variable-length) | yes |
| Hard links (`$FILE_NAME` fan-out + link-count) | yes |
| ADS write / delete (resident + promote) | yes |
| Reparse point write / remove + symlink create | yes |
| EA write / remove | yes |
| Timestamp writes (atime / mtime / ctime / crtime) | yes |
| File-attribute flag toggling | yes |
| mkfs (format a blank image to NTFS) | yes |
| fsck (clear dirty flag + `$LogFile` reset) | yes |
| `$INDEX_ALLOCATION` insert / delete (overflowed dirs) | not yet — W3.2 / W3.3 |
| `$MFT` self-growth (full `$MFT:$Bitmap`) | not yet — W2.6 |

### NTFS feature coverage

| Feature | Read | Write |
|---|---|---|
| Resident attributes | yes | yes |
| Non-resident attributes | yes | yes |
| `$INDEX_ROOT` directories | yes | yes |
| `$INDEX_ALLOCATION` directories (B+ tree) | yes | partial — read-traversal yes, insert / delete not implemented |
| Alternate Data Streams | yes | yes (resident + auto-promote) |
| Reparse points (symlinks, junctions, generic) | yes | yes |
| Extended Attributes (`$EA`) | yes | yes |
| Unicode names (UTF-16, up to 255 chars) | yes | yes |
| Case-folding (`$UpCase` collation) | yes | yes (Microsoft canonical NT 3.x table baked in) |
| Compressed `$DATA` | partial — uncompressed runs only | no |
| Sparse `$DATA` | partial — non-hole reads only | no |
| Encrypted `$DATA` (EFS) | no | no |
| Attribute lists (`$ATTRIBUTE_LIST`) | no — records that overflow are rejected | no |
| USN journal (`$UsnJrnl`) updates | n/a | no |
| Transactional NTFS (TxF) | n/a | no — deprecated by Microsoft |
| Volume resize (`$Bitmap` grow/shrink) | n/a | no |

## What works

Concrete user-observable list, end-to-end:

- Mount an NTFS image or `/dev/diskN` and walk its tree.
- Format a blank image as NTFS, mount it on Windows, write to it
  through `ntfs.sys`, and pass `chkdsk /scan` (read-only validation).
- Replace a small file's contents and have it stay resident; replace
  a small file's contents with a large blob and have it auto-promote
  to non-resident with cluster allocation against `$Bitmap`.
- Create / delete files and directories under any parent that fits
  in `$INDEX_ROOT` (the typical case for fresh and lightly-populated
  volumes).
- Rename a file with either same UTF-16 length (fast in-place patch)
  or variable length (re-inserts the index entry).
- Add and remove hard links; link counts and `$FILE_NAME` records
  stay consistent.
- Add / remove ADS, reparse points, EAs; create symlinks.
- Patch any combination of the four NT timestamps in
  `$STANDARD_INFORMATION`; toggle `FILE_ATTRIBUTE_*` flags.
- Detect a dirty volume and clean it (`fs_ntfs_fsck`) — including
  through a callback-only block-device transport with progress
  callbacks for long `$LogFile` resets.
- Drive everything from C, Go (cgo), or Swift via the stable
  `fs_ntfs_*` C ABI in `include/fs_ntfs.h`.

## What doesn't work

Specific limits, current as of HEAD:

- **Overflowed directories.** Once a directory has more entries
  than fit in `$INDEX_ROOT`, writes that would touch its index
  (`create_file`, `mkdir`, `rmdir`, `unlink`, `rename`) refuse with
  a fail-fast error. Reads through such directories do work.
- **Full `$MFT`.** When `$MFT:$Bitmap` is exhausted, `create_file`
  and `mkdir` fail. Self-growing `$MFT` is the W2.6 work item.
- **Compressed `$DATA` writes.** Reading non-compressed extents is
  fine; any write to a compressed file is refused.
- **Encrypted (EFS) data.** Not implemented either way.
- **Sparse-aware writes.** Writes assume plain ranges; reads of
  hole regions do return zero, but writing into a hole won't
  re-encode the run list.
- **`$AttributeList` overflow.** MFT records that have spilled into
  an attribute list are rejected on read. Affects very fragmented
  files on old, heavily churned volumes.
- **USN journal updates.** Mutations are not reflected in
  `$UsnJrnl`.
- **Transactional NTFS (TxF).** Deprecated by Microsoft; not a goal.
- **Volume resize.** This crate expects a pre-sized image.
- **Disk-level operations.** No partitioning. mkfs operates on a
  pre-existing partition or raw image.

## Scenario field translations (NTFS ↔ harness)

The test matrix uses harness-level generic field names rather than
NTFS-native terminology, so the same scenario shape composes with
sibling fs-* drivers. If you came here looking for an NTFS-native
name and can't find it in `test-matrix.json`, this table is where
to look:

| NTFS-native name | Harness name      | Notes |
|------------------|-------------------|-------|
| `cluster_size`   | `alloc_unit_size` | Same concept, generic name. Valid: 512, 1024, 2048, 4096, ..., 65536. The legacy `cluster_size` key is still accepted as a serde alias during the v1->v2 transition. |
| chkdsk verdict   | `verdict_shape`   | Mapped to `clean` / `repair-ok` / `repair-required`. |
| operation_sequence string (v1) | `recipe[]` array (v2) | The v1 arrow-string `mac:format -> win:chkdsk(readonly,/scan)` becomes a v2 recipe of typed steps with `host: "host" / "vm"` per step. |

Cross-driver vocabulary index lives in
[`vendor/fs-test-harness/docs/vocabulary.md`](vendor/fs-test-harness/docs/vocabulary.md);
contributor-facing translation rules + bloat-prevention conventions
are documented there.

## Test contract

- **Unit tests:** 2 (in-tree, `cargo test --lib`).
- **Integration tests:** 396 passing across 66 binaries (one
  failing as of HEAD, in `mkfs_roundtrip::format_and_parse_back`,
  tracked in the test matrix). 42 ignored (mostly `cluster_size_matrix`
  long-running and the matrix harness on non-Windows hosts).
- **Coverage areas (one or more dedicated test files each):**
  reads, every C-ABI entry point, round-trip writes, corruption
  fuzz under concurrent read-back, Unicode names, large files,
  sparse reads, deep directories, rename / resize variants, fsck
  on both path and callback transports, ADS combinatorics, EA
  combinatorics, end-to-end mac→Windows workflows.
- **Static images** for fixtures awkward to build programmatically
  live under `test-disks/`. Most tests assemble their NTFS image at
  runtime.
- **chkdsk validation:** the `test-matrix.json` matrix runs through
  the vendored `fs-test-harness` runner (see
  `vendor/fs-test-harness/scripts/test-windows-matrix.sh`), which on Windows
  shells out to `rust-ntfs format`, Microsoft's `format.com`, and
  Microsoft's `chkdsk` to validate every formatted image. On non-
  Windows hosts the matrix tests are reported as ignored. Microsoft's
  `chkdsk` is the authoritative validator — not byte-equivalence with
  any third-party formatter.
- **Test matrix:** `test-matrix.json` at repo root carries 42
  scenarios. The harness drives mac-side ops (format, populate via
  the pure-Rust write API) and Windows-side ops (mount, chkdsk,
  enumerate, write, repeat-mount stability cycles) through a single
  declarative JSON contract; results from a Windows VM stream back
  over SSH. See `harness.toml` for the op declarations and
  `vendor/fs-test-harness/` for the runner.
- **Fuzz:** `fuzz/` carries cargo-fuzz harnesses for the three
  byte-decoders most likely to regress (data-runs, attribute headers,
  INDX block headers).
- **Bench:** `benches/byte_decoders.rs` (Criterion). Not part of CI.

## Roadmap

- [ ] **W2.6** — `$MFT` self-growth when `$MFT:$Bitmap` exhausts.
  Unblocks `create_file` / `mkdir` on volumes whose initial MFT
  reservation is full.
- [ ] **W3.2** — `$INDEX_ALLOCATION` B+ tree insert with split +
  promotion from resident `$INDEX_ROOT`. Unblocks all writes against
  overflowed directory parents.
- [ ] **W3.3** — `$INDEX_ALLOCATION` B+ tree delete with rebalance.
  Symmetric to W3.2; needed by `rmdir` / `unlink` / `rename`-out
  on overflowed parents.
- [ ] **W3 fixtures** — empty / deep / full B+ tree images for
  exercising W3.2 / W3.3 splits and merges.
- [ ] **mkfs hardening** — close out the remaining `failed` and
  `pending` matrix scenarios in `test-matrix.json`.
- [ ] **`$AttributeList` support** — at least on the read side so
  heavily fragmented files don't get rejected.
- [ ] **Compressed-read support** — `LZNT1` decompression so files
  written by Windows with compression enabled can be read back.
- [ ] Cut a 0.2 release tag once W2.6 + W3.2 + W3.3 land.

## Changelog

Reverse chronological highlights from `git log`. Full per-commit
history available via `git log` in the repo.

### 2026-05-24

- `feat(read)`: `read_reparse_point` exposes raw `(tag, data)` for
  any reparse type (`fs_ntfs_readlink` only handles symlinks/mount
  points). Useful for inspecting third-party tags (dedup, HSM, etc.).
- `feat(read)`: `list_named_streams` enumerates the names of every
  named `$DATA` attribute (ADS) on a file, excluding the unnamed
  primary stream.
- `feat(read)`: `list_ea_keys` returns just the EA names, skipping
  values up to 64KB each — for callers that only need to discover
  which EAs exist.
- `feat(read)`: `read_si_full` surfaces every MS-FSCC §2.4.2
  `$STANDARD_INFORMATION` field, including the NTFS 3.x trailer
  (owner_id, security_id, quota, usn) when present.
- `feat(capi)`: `fs_ntfs_set_object_id_extended_h` — handle-based
  sibling of `fs_ntfs_write_object_id_extended` for callers holding
  an open filesystem handle.
- `fix(pr#49)`: bundle of 23 review-feedback fixes covering
  `fn_namespace_for` DOS 8.3 validity, `read_volume_label` odd-byte
  corruption surfacing, `read_object_id_extended` strict `{16, 64}`
  length validation, `remove_attribute_at` bounds-checked helper
  replacing five `copy_within` panic sites, `_pad[3]` → `_pad[5]`
  explicit padding, smoke-mode gate failure-detection, dirty-tree
  seal false-positive guard, VM-host redaction, PowerShell short-
  read detection, portable verdict-collect path, cargo-fmt
  hygiene. See `changelog.md` for the full per-item list.
- `chore(hooks)`: `core.hooksPath` set to `.githooks` so
  `cargo fmt --check` + `cargo clippy -- -D warnings` run locally
  before commit (one-shot install:
  `bash scripts/install-hooks.sh`).

### 2026-05-23

- `feat(read)`: `read_attributes` / `describe_attributes` — list every
  attribute on a file's MFT record (type code + name + dimensions).
  Diagnostics helper for chkdsk byte-diff work.
- `feat(read)`: `read_file_names` returns every `$FILE_NAME` on a
  file (multi-namespace files surface as multiple records).
- `feat(read)`: `read_volume_label` decodes `$VOLUME_NAME` to UTF-8.
- `feat(write)`: `set_volume_label` renames or clears the volume
  `$VOLUME_NAME`. Empty label removes the attribute.
- `feat(write)`: `set_security_id` points a file at an existing
  `$Secure:$SDS` entry. Read counterpart `read_security_id` also
  shipped.
- `feat(write)`: `write_object_id` runtime 16-byte `$OBJECT_ID`
  writer; `write_object_id_extended` covers the 64-byte
  object_id + Birth GUIDs form (MS-FSCC §2.4.6).
- `feat(volume)`: v2 of the volume-info struct adding
  `volume_flags` / `is_dirty` / `mft_record_size` / `bytes_per_sector`
  (`fs_ntfs_get_volume_info_v2`). v1 fields stay at the same offsets
  so legacy callers still work — verified by compile-time
  `offset_of!` assertion tests.
- `feat(index_io)`: `compare_names_ordinal` primitive for
  case-sensitive collation (foundation for `CASE_SENSITIVE_DIR`
  work; not wired into the default code path yet).
- `feat(test)`: seal-by-binary-hash matrix discipline
  (`scripts/matrix-baseline.sh` writes
  `test-diagnostics/matrix-results.json` with
  `binary_sha256 = sha256(target/release/rust-ntfs)`; survives
  rebase/squash-merge via `--remap-path-prefix`).
  42/42 sealed runs recorded for staging tip `30fcdd6` (11369s) and
  staging-2 tip `d9595c7` (13794s).

### 2026-05-21

- `chore(vendor)`: `am-fs-core` is now a git submodule at
  `vendor/rust-fs-core` rather than an unmanaged `../rust-fs-core`
  sibling path. Clone with `--recurse-submodules` (or
  `git submodule update --init --recursive`) and the crate is
  self-buildable — no side-by-side checkout required.
- `fix(facade)`: `Filesystem::volume_info()` now reads the real
  `$VOLUME_INFORMATION` bytes via upstream `ntfs.volume_info()`
  instead of returning a hardcoded `(3, 1)`. Fresh-format volumes
  correctly report `(1, 2)` until `ntfs.sys` upgrades them on first
  mount.
- `feat(fsck)`: `upgrade_volume_version` rewrites a fresh-format
  `1.2 + UPGRADE_ON_MOUNT` volume to `3.1` with the flag cleared —
  the same transition `ntfs.sys` does on first RW mount. Now wired
  into **every** RW entry point: `fs_ntfs_mount_rw_with_fs_core_device`,
  `fs_ntfs_mount` (path-based), `fs_ntfs_mount_with_callbacks` (when
  the caller supplied a `write` cb), and the new
  `Filesystem::mount_rw()` used by the `rust-ntfs` CLI's mutating
  commands (`touch`, `mkdir`, `rm`, `rmdir`, `write`). Best-effort
  throughout — failure is logged at `warn` and never fails the mount.

### 2026-05-03

- `feat(observability)`: lifecycle events log via the `log` facade.
- `feat(test-matrix)`: chkdsk `/F` repair-lane verdict shapes;
  win:write fixture support; repeat-mount harness for Tier-3
  stability cycles; mac-prefix dispatcher for pure-mac scenario
  chains; 10 new Tier-1 scenarios.
- `feat(fsck,cli)`: `set_dirty` symmetric to `clear_dirty`.
- `refactor(cli)`: 4 separate bins consolidated into a single
  `rust-ntfs` binary with subcommands.
- `feat(api,ci)`: dirent name widened to 1024 bytes; ASan smoke
  job in CI; cargo-deny licence enforcement; macOS runner added
  to CI matrix.
- `test(fuzz)`: cargo-fuzz harnesses for the three byte-decoders.
- `test(bench)`: Criterion harness for byte-decoders.
- `chore(license)`: GPL-tooling citations in source replaced with
  Microsoft MS-FSCC / Windows Internals references throughout.

### 2026-05-02 — mkfs mount + write breakthrough

After a multi-month chkdsk run hitting the same `frs.cxx 60f` ceiling,
the test contract was switched from "chkdsk-clean" to "mounts on
Windows + accepts a real write" — and on **2026-05-02** images
produced by the pure-Rust mkfs path mounted under `ntfs.sys` and
took writes from a Windows host for the first time. Contributing
fixes:

- `mkfs`: `$STANDARD_INFORMATION` 48-byte NTFS-1.x form on system
  records (commit `7072242`).
- `mkfs`: bake Microsoft's canonical NT 3.x `$UpCase` table verbatim,
  replacing the runtime generator (`d620205`).
- `mkfs`: `$SECURITY_DESCRIPTOR` (0x50) on every system MFT record
  (`091848d`); subsequently refined — adding it everywhere broke
  mount, the layout-order-correct version landed (`f2677d3`).
- `mkfs`: rec 11 left empty to match Microsoft's reserved-slot
  layout (`26b1a02`).
- `mkfs`: `file_attributes 0x06` on system `$STANDARD_INFORMATION`
  (drop ARCHIVE) (`faaff9c`).
- `infra(test)`: explicit cluster size pinned on the reference VHDX
  format so the byte-diff has a stable baseline (`23ed755`).
- Root `$I30` populated with system-file entries (`1c5007a`).

### 2026-05-01

- `feat(bin)`: `mkfs_ntfs --create-size` one-shot create+format for
  image files (`7c9f1d7`).
- `ci`: image wrapped in a GPT-partitioned fixed VHDX so Windows
  `Mount-DiskImage` accepts it (`9af3146`, `c02c703`).
- `ci+docs`: parallel reference-NTFS format + byte-diff comparison
  in CI.

### 2026-04-30

- `feat`: handle-based `_h` mutation siblings for the callback-mount
  read/write path (`7cd4020`).

### 2026-04-19 .. 2026-04-30 — write surface (W1 → W4)

Original write code shipped in stages, each with its own
integration suite under `tests/`:

- W1 — in-place writes against existing non-resident `$DATA`.
- W2 — resident → non-resident promotion + cluster allocation.
- W2.1 — `grow` / `truncate`.
- W2.2 — resident-resize machinery.
- W2.3 — `$Bitmap` cluster allocator.
- W2.4 — `$MFT:$Bitmap` MFT-record allocator.
- W2.5 — FILE-record assembly + fixup arrays + `create_file`,
  `mkdir`, `unlink`, `rmdir`, `rename`, `link`.
- W3.1 — index entry insert / remove inside `$INDEX_ROOT`.
- W4.1 — reparse points + symlinks.
- W4.2 — alternate data streams.
- W4.3 — extended attributes.
- W4.4 — file-attribute flag toggling and timestamp writes.

### 2026-04-20 — 0.1.2

Docs / packaging release. README rewritten with capability matrix
contrasting fs-ntfs against upstream `ntfs = "0.4"`. `Cargo.toml`
description neutralised (no longer Swift/FSKit-specific).

### 2026-04-20 — 0.1.1

Callback-based fsck. New: `fs_ntfs_blockdev_cfg_t.write` field;
`fs_ntfs_is_dirty_with_callbacks`; `fs_ntfs_fsck_with_callbacks`
with progress callbacks (`reset_logfile` / `clear_dirty` phases).

### 2026-04-18 — 0.1.0 (unreleased) initial commit

C-ABI wrapper around `ColinFinck/ntfs` (read-only). Lifecycle,
`stat`, `readdir`, `read_file`, `last_error`. Extracted from the
archived `ntfsbridge/` crate.

## License

Dual-licensed Apache-2.0 ([`LICENSE-APACHE`](LICENSE-APACHE)) or
MIT ([`LICENSE-MIT`](LICENSE-MIT)) at your option, matching the
upstream `ntfs` crate. The standard Rust-ecosystem dual-license
pattern.

`Cargo.toml` declares `license = "MIT OR Apache-2.0"`.
`deny.toml` enforces a permissive-only allowlist (MIT, Apache-2.0,
BSD-2-Clause, BSD-3-Clause, ISC, Unicode, CC0-1.0, Zlib, 0BSD)
across every transitive dep — no GPL / LGPL / MPL / AGPL ever
enters the dependency graph.

## Building

Standard cargo:

```sh
cargo build --release
# → target/release/libfs_ntfs.{a,rlib}
# → target/release/rust-ntfs   (CLI: format / ls / touch / mkdir / write / rm / rmdir / set_dirty)
```

Universal macOS static lib (aarch64 + x86_64 lipo'd):

```sh
./build.sh           # → dist/libfs_ntfs.a
```

The Rust toolchain is pinned in `rust-toolchain.toml`; cargo picks
it up automatically.

### Tests

```sh
cargo test                     # unit + integration (skips matrix on non-Windows)
cargo test --test capi_fsck_callbacks
cargo test --lib               # unit only
```

### Test matrix (Windows + macOS VM coordination)

The chkdsk-validated matrix lives in `test-matrix.json` at the repo
root. The matrix runs through the vendored `fs-test-harness` runner.
Drivers:

- `scripts/setup-windows-vm.sh` / `.ps1` — bootstrap a Windows VM
  with the toolchain needed to run `format.com` / `chkdsk` plus
  `vhd_tool` for the wrapper-image lifecycle.
- `vendor/fs-test-harness/scripts/test-windows-matrix.sh` — orchestrator that
  tars the consumer source, SSHes to the VM, invokes the harness's
  `run-matrix` runner, and pulls per-scenario diag back to the Mac.
- `vendor/fs-test-harness/scripts/claim-scenario.sh`,
  `vendor/fs-test-harness/scripts/update-scenario-status.sh`,
  `vendor/fs-test-harness/scripts/reset-non-passed.sh` — generic, FS-agnostic
  state-machine over `test-matrix.json`. Vendored from
  `antimatter-studios/fs-test-harness`.

Agent coordination rules: see
[`docs/multi-agent-test-protocol.md`](docs/multi-agent-test-protocol.md)
(historical — describes the v1 matrix flow; some script names have
moved into `vendor/fs-test-harness/scripts/`).

### Pre-commit hooks

One-time per clone — runs the same `cargo fmt --check` and
`cargo clippy --all-targets -- -D warnings` checks CI runs:

```sh
./scripts/install-hooks.sh
```

Bypass a single commit with `git commit --no-verify`.

## Using from C

Link `libfs_ntfs.a` and include `fs_ntfs.h`:

```c
#include "fs_ntfs.h"

fs_ntfs_fs_t *fs = fs_ntfs_mount("/path/to/ntfs.img");
if (!fs) {
    fprintf(stderr, "mount: %s\n", fs_ntfs_last_error());
    return 1;
}

fs_ntfs_attr_t attr;
if (fs_ntfs_stat(fs, "/readme.txt", &attr) == 0) {
    printf("size=%llu\n", (unsigned long long)attr.size);
}

fs_ntfs_umount(fs);
```

Callback-based mount for hosts that can't open the device directly
(sandboxed FSKit extensions, out-of-process backends):

```c
static int my_read(void *ctx, void *buf, uint64_t off, uint64_t len) { /* … */ }

fs_ntfs_blockdev_cfg_t cfg = {
    .read       = my_read,
    .context    = my_context,
    .size_bytes = total_bytes,
    /* .write left NULL for a read-only mount */
};

fs_ntfs_fs_t *fs = fs_ntfs_mount_with_callbacks(&cfg);
```

End-to-end fsck against a callback-held block device, with progress:

```c
static int on_progress(void *ctx, const char *phase,
                       uint64_t done, uint64_t total) { /* … */ return 0; }

uint64_t bytes = 0;
uint8_t  cleared = 0;
int rc = fs_ntfs_fsck_with_callbacks(&cfg, on_progress, NULL,
                                     &bytes, &cleared);
```

Full ABI surface: `include/fs_ntfs.h`. Implementation entry point:
`src/lib.rs`.

## Using from Rust

The C ABI is the primary surface. If you're writing pure Rust and
only need read support, depend on upstream directly:

```toml
[dependencies]
ntfs = "0.4"
```

If you want fs-ntfs's write API from Rust, pull the crate in as a
path or git dependency and use the modules under `facade::` and
`write::`. Note that the Rust facade re-parses the boot sector and
MFT per call (it's stateless by design); for hot paths, go through
the C ABI which keeps the parsed volume in memory.

## Credits

Read parsing is the work of [Colin Finck](https://github.com/ColinFinck)
and his [`ntfs`](https://github.com/ColinFinck/ntfs) crate — this
crate depends on it unchanged for every MFT, attribute, and index
read. The write, recovery, mkfs, and FFI code in this crate is
original work, layered on top of that reader. Citations throughout
are Microsoft MS-FSCC and Windows Internals 7th ed. only; no GPL'd
NTFS reimplementation was consulted at any point.

## Disclaimer — use at your own risk

**Read this before pointing the crate at anything you care about.**

This is experimental filesystem code that reads *and writes* the
on-disk structures of live filesystems. Bugs in this class of code
can — and sooner or later will — corrupt or destroy data. The
Apache-2.0 / MIT license above already contains the standard
no-warranty and limitation-of-liability clauses; this section
restates them in plain English so there is no ambiguity about what
you are agreeing to when you use the software.

**By using this software you accept that:**

- The author(s) and contributors provide this crate **as is**, with
  **no warranty of any kind**, express or implied — including but
  not limited to warranties of merchantability, fitness for a
  particular purpose, correctness, data integrity, durability,
  security, or non-infringement.
- The author(s) and contributors are **not liable** for any loss,
  damage, or expense of any kind arising out of or related to your
  use of the software. This explicitly includes (non-exhaustively)
  lost or corrupted data, corrupted filesystems, volumes that will
  no longer mount, hardware damage, downtime, lost revenue, missed
  deadlines, support costs, or any direct, indirect, incidental,
  special, consequential, or punitive damages — regardless of the
  legal theory under which such damages might be sought.
- You are **solely responsible** for backing up any data that could
  be touched by this software *before* running it. The only safe
  workflow when experimenting with an unofficial filesystem driver
  is: work on disk *images* or on *copies*, never on your only
  copy of anything irreplaceable.
- If that is not acceptable to you, **do not use this software**.

This disclaimer is a plain-English restatement of the license terms
above, not a separate license. The license terms apply in full.
