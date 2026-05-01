# Local Windows test pipeline

A local mirror of the GitHub Actions `validate-mkfs-windows` job. Lets
us iterate on `mkfs_ntfs` against Microsoft's `chkdsk` in **~30-90 s**
per cycle instead of the ~2-4 min CI roundtrip, and without burning
Actions minutes on every guess.

## What it does

Every run, end-to-end:

1. Tar the source tree from the Mac and stream it via SSH onto the
   Windows VM.
2. SSH in, build `mkfs_ntfs.exe` against the gnullvm toolchain.
3. Format an `nfs.img`, wrap in a GPT-partitioned VHDX (same layout
   `diskutil eraseDisk` produces on macOS), mount.
4. **Format a parallel reference VHDX with Microsoft's own
   `format.com /FS:NTFS`** -- this is the canonical reference our
   output is compared against (see [chkdsk-findings.md](./chkdsk-findings.md)).
5. Dump our boot sector + first 16 MFT records and the reference's
   into `diag/*.bin` for byte-diff analysis.
6. Run `chkdsk DRIVE:` (read-only) and `chkdsk DRIVE: /scan`.
7. Tar the `diag/` tree back to the Mac into a per-iteration tmp dir.

The pipeline is the corroboration mechanism for the `corroborated-debug`
skill -- every change to `mkfs_ntfs` must be backed by a byte-diff
between our output and the Microsoft reference produced here.

## One-time setup

On a fresh Windows ARM64 VM:

```sh
bash scripts/setup-windows-vm.sh
```

This runs `setup-windows-vm.ps1` over SSH which installs (idempotently):

| Component | Why we need it | Size |
|---|---|---|
| `Rustlang.Rustup` | rustup itself | ~50 MB |
| `stable-aarch64-pc-windows-gnullvm` | Rust toolchain that doesn't need Visual C++ Build Tools | ~600 MB |
| `MartinStorsjo.LLVM-MinGW.UCRT` | `aarch64-w64-mingw32-clang.exe`, the linker the gnullvm target requires for build scripts | ~250 MB |
| `cloudbase.qemu-img` | Creates the GPT-partitioned VHDX wrapper | ~10 MB |

Total: ~900 MB, ~3 minutes over a typical connection.

### Why gnullvm instead of MSVC

The two Rust-on-Windows targets are `aarch64-pc-windows-msvc` and
`aarch64-pc-windows-gnullvm`. We picked the latter because:

- MSVC requires Visual Studio Build Tools (~3 GB) and Microsoft's
  commercial license acceptance.
- gnullvm uses LLVM tooling end-to-end -- clean license, smaller
  footprint, identical output for our use case (mkfs_ntfs is pure
  Rust + `std::fs`, no Windows-API calls).

The only catch: rustup's gnullvm component ships `rust-lld` but
expects an external `aarch64-w64-mingw32-clang` for build scripts.
That's what the LLVM-MinGW package provides.

## Per-iteration usage

```sh
bash scripts/test-windows-local.sh
```

Outputs:

- `chkdsk` verdict to stdout (read-only + /scan exit codes).
- `${TMPDIR}/rust-fs-ntfs-diag/iter-<timestamp>/` -- full `diag/` tree
  with everything CI captures (boot sector hex, BPB decode, MFT
  records from both ours and the Microsoft reference, chkdsk output,
  Windows Event Log NTFS entries).

The diag dir is in `$TMPDIR` (not the repo) so it never leaks into
git. `.gitignore` also excludes any in-tree `diag/` or `*.vhdx` as
belt-and-braces.

### Overrides

```sh
VM_HOST=user@otherhost bash scripts/test-windows-local.sh
VM_WORKDIR=D:/work/rust-fs-ntfs bash scripts/test-windows-local.sh
DIAG_DIR=~/diags bash scripts/test-windows-local.sh
```

## Using the diag for byte-diff analysis

The two binary files that matter most:

- `diag/ours-mft-16recs.bin` -- 16 × 4 KiB MFT records from our
  `mkfs_ntfs` output, starting at our `$MFT` location.
- `diag/reference-mft-16recs.bin` -- same 16 records from the
  Microsoft `format.com` reference.

Compare them per-record with Python or `cmp`. The findings doc has
worked examples (see iter6-iter11).

## Composition with skills

This pipeline is the local-iteration backend for:

- **`corroborated-debug`** -- provides the parallel reference output
  needed for byte-diff evidence.
- **`dev-loop`** -- Phase 1's baseline test contract should include
  "Stage 1 of chkdsk passes clean" as the equivalent of `cargo test`.
- **`documentation-protocol`** -- each iteration's `diag/` dir is the
  evidence packet for the iteration entry in
  [chkdsk-findings.md](./chkdsk-findings.md).

## When to use CI vs local

- **Local pipeline**: every iteration of the corroborated-debug loop.
  The iteration speed (10-30s vs 2 min) is what makes byte-diff-driven
  debugging viable.
- **CI**: PR validation, releases, anything that needs a clean
  reproducible environment. CI is the contract; local is the
  workshop.

If a fix passes locally but fails CI, the most likely cause is local
state on the VM (stale `target/`, lingering mounts). `scripts/test-windows-local.sh`
clears `diag/`, `nfs.img`, `wrapper.vhdx`, `reference.vhdx` at the
start of every run; `target/` is left intact for incremental compile
speed but can be wiped with
`ssh $VM_HOST 'Remove-Item -Recurse -Force C:\Users\chris\dev\rust-fs-ntfs\target'`.
