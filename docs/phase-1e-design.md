# Phase 1e — Replace `qemu-img` with `am-img-vhd`

Design note. Captures what's involved in dropping the `qemu-img` runtime
dependency from `scripts/v2/_lib.ps1` in favour of the in-house
`am-img-vhd` writer. Three viable paths are sketched below; pick one in
the morning and implement.

## What the dependency looks like today

`scripts/v2/_lib.ps1::Initialize-VhdxFromImg` invokes `qemu-img` once
per scenario:

```powershell
& qemu-img create -f vhdx -o subformat=fixed $Vhdx "${wrapperMb}M" *> "$Diag\wrapper-create.txt"
```

Effects:
- Creates a `.vhdx` file at `$Vhdx` with `wrapperMb` MiB of
  preallocated capacity, fixed (non-sparse) layout.
- Output captured to `wrapper-create.txt` for diag.
- `qemu-img` is GPL-licensed; bundled into the test VM via
  `setup-windows-vm.ps1`'s package install. Every scenario's win-side
  step shells out to it.

## Why replace it

- **Licensing posture.** Avoid name-binding to a GPL CLI tool in
  test infrastructure (per the no-GPL-tool-name memory).
- **Determinism.** `am-img-vhd::create_fixed` is in-house; we own
  the VHD format details and can fix or extend them without bumping
  qemu-img.
- **Footprint.** One less Windows-side package on the test VM.
- **Self-test.** `am-img-vhd` already exercises VHD reads under our
  own tests; using its writer in this pipeline is incidental
  validation of its write path.

## Format note: VHD vs VHDX

`am-img-vhd` writes the older **VHD** format (Microsoft VHD, 1.0).
qemu-img today writes **VHDX** (the newer format). Both are mountable
by `Mount-DiskImage` on every supported Windows version; functionally
interchangeable for our purposes. Switching format means:
- `Get-VhdxPathFor` becomes `Get-VhdPathFor`, returns `.vhd` instead
  of `.vhdx`.
- File-extension references in cleanup, diag, and (a few) helper-
  script names update accordingly.
- All the mount/dismount/partition/raw-write code is unchanged
  (`Mount-DiskImage` is format-agnostic).
- `fsutil sparse setflag <file> 0` works on `.vhd` too (fsutil
  doesn't care about the wrapper format).

VHDX has some advantages over VHD (4 KiB sector support, larger max
size, log-based crash recovery). For a test wrapper that gets
created+populated+chkdsked+thrown away, none of those matter. The
4 GiB and 16 GiB scenarios fit within VHD's 2040 GiB cap with room to
spare.

## The cross-compile problem

`am-img-vhd` exposes the writer as a CLI binary `vhd_tool` (see
`vendor/rust-img-vhd/src/bin/vhd_tool.rs`):

```
vhd_tool create-fixed <file> <size>
```

To run that on the Windows ARM64 VM, we need `vhd_tool.exe` compiled
for `aarch64-pc-windows-gnullvm`. The current Mac-side build pipeline
does NOT cross-compile anything for Windows — `rust-ntfs` only ever
runs on the host. The VM has its own Rust toolchain (per
`harness.toml::vm.rust_toolchain = "stable-aarch64-pc-windows-gnullvm"`)
but it's not invoked in the existing flow.

## Three viable paths

### Path A — cross-compile on the Mac, ship the binary

1. Add `aarch64-pc-windows-gnullvm` Rust target to the host
   (`rustup target add aarch64-pc-windows-gnullvm`) plus the
   GNU LLVM toolchain for Windows ARM (`brew install llvm` +
   appropriate linker config).
2. Add a step to `scripts/v2/smoke.sh` (and the bulk-validation
   driver) that builds `vhd_tool` for the Windows target:
   ```
   cargo build --manifest-path ../rust-img-vhd/Cargo.toml \
     --release --target aarch64-pc-windows-gnullvm --bin vhd_tool
   ```
3. Ship `vhd_tool.exe` to the VM as part of the source-ship phase
   (or to a known PATH location like `C:\Tools\vhd_tool.exe`).
4. Modify `_lib.ps1`:
   - `Get-VhdxPathFor` → `.vhd` extension
   - `qemu-img create -f vhdx ...` → `& vhd_tool create-fixed $Vhd $sizeBytes`
   - Drop the `fsutil sparse setflag` line (or keep it; harmless on
     a fixed VHD).
5. Update every `*.vhdx` reference in `_lib.ps1`, the per-op scripts,
   and the cleanup paths.

**Pros:** clean. Build pipeline gets a Windows-target step (useful
for any future VM-side Rust binary). The shipped binary is a static
artifact — VM doesn't need a Rust toolchain to use it.
**Cons:** one-time host setup is non-trivial (target install +
Windows linker config). CI would also need it.

### Path B — build on the VM as part of setup

1. Vendor `rust-img-vhd` into the rust-fs-ntfs source tree (or rely
   on the existing sibling layout + extend the source-ship to include
   it).
2. Add a one-time setup step that runs `cargo build --release --bin
   vhd_tool` on the VM (uses the VM's existing Rust toolchain).
3. Same `_lib.ps1` rewrite as Path A.

**Pros:** no host-side toolchain change. The VM already has Rust.
**Cons:** ship-source phase grows (an extra crate's worth of source).
First-time build on the VM takes a few minutes; subsequent
incrementals are fast. Couples rust-fs-ntfs to rust-img-vhd at a
repo-layout level.

### Path C — drop VHD wrap entirely (not recommended)

Mount the `.img` directly. Windows can't mount raw `.img` via
`Mount-DiskImage` (it's a VHD/VHDX-only API), so this would require:
- Either invoking `imdisk` (third-party + not bundled)
- Or writing a VHDX header in-place at the front of the `.img`,
  then mounting it as a VHDX (corrupts the .img for non-Windows
  consumers; ugly)

**Don't do this.** The wrap is structural to how Windows mounts
disk images. Better to fix the wrap, not eliminate it.

## Recommendation

**Path B** is the lowest-friction next step:
- The VM already has the Rust toolchain.
- No host build-system changes.
- Source-ship grows by ~1 MB (rust-img-vhd is small); negligible.
- Easy to revert if it doesn't work out — just put `qemu-img` back.

If/when the project gains other Windows-target binaries
(e.g., a bundled `vhd_tool` for distribution), Path A becomes
worth the host-side investment.

## Out of scope for tonight

This doc doesn't change any code. The `qemu-img` invocation in
`_lib.ps1:71` continues to work as before. Phase 1e's actual
implementation needs the user to pick a path in the morning.

## Adjacent: VHDX → VHD migration impact on prior PRs

If we go with VHD format (Path A or B), every `.vhdx` reference in
the v2 scripts becomes `.vhd`. The PR that lands Phase 1e should
include a sed-style sweep across `scripts/v2/*.ps1` and the cleanup
paths in `_lib.ps1::Dismount-VhdxAndCleanup`. No behavioural change —
just file-extension consistency.

The Phase 1e header comment in every `scripts/v2/*.ps1` file
("Phase 1e replacement target") should also be updated to point at
this doc once the design is settled.
