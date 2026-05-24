# Procmon trace of `chkdsk /scan` — investigating the /scan exit 13 ceiling

Background: [`docs/FUTURE_FEATURES.md` §3.1](./FUTURE_FEATURES.md). Our
mkfs output passes `chkdsk` read-only and `chkdsk /F` (offline) cleanly
but `chkdsk /scan` (online) consistently exits 13 ("errors queued for
offline repair") on every mkfs scenario, while a reference volume from
Microsoft `format.com` exits 0 on the same scan. We've ruled out ~11
plausible structural-layout hypotheses (see
[`docs/chkdsk-improvement-findings.md`](./chkdsk-improvement-findings.md)
§3); the productive next move per §3.1's "productive next moves #1" is
to capture every disk read `chkdsk /scan` performs against our volume
via Procmon and correlate the read offsets with where our bytes diverge
from `format.com`'s.

This doc describes the harness for that capture.

## Prereqs on the Windows VM

- Windows 10/11.
- PowerShell ≥ 5.
- `vhd_tool.exe` on PATH (installed by `scripts/setup-windows-vm.ps1`
  via `cargo install` from `antimatter-studios/rust-img-vhd`).
- Administrator (the script's VHD mount + raw `\\.\PhysicalDrive`
  write + ETW kernel-session start all need elevation).

The capture itself is driven by `wpr.exe` (Windows Performance
Recorder, built into Windows 10/11). No external download required —
Procmon's user-mode GUI app exits early under a non-interactive SSH
session, which is why this script uses `wpr` instead.

## End-to-end flow

Set environment variables for your VM up front and reuse them in
the SSH/SCP commands below. Nothing here is checked into the repo;
treat the host/user as private to your environment.

```bash
# Adjust to your environment.
export WIN_VM_USER=youruser
export WIN_VM_HOST=192.0.2.10            # your Windows VM's IP/hostname
export WIN_VM_KEY=~/.ssh/win-vm-key      # path to the private key

# 1. Build rust-ntfs locally and produce an nfs.img to send to the VM.
cargo build --release --bin rust-ntfs
mkdir -p /tmp/procmon-input
./target/release/rust-ntfs format -L CITEST --serial deadbeefcafe1234 \
    --create-size 256M /tmp/procmon-input/nfs.img

# 2. Copy the image + the trace script to the VM.
scp -i "$WIN_VM_KEY" \
    /tmp/procmon-input/nfs.img scripts/procmon-chkdsk-trace.ps1 \
    "$WIN_VM_USER@$WIN_VM_HOST":C:/trace/

# 3. Run the trace.
ssh -i "$WIN_VM_KEY" "$WIN_VM_USER@$WIN_VM_HOST" \
    'powershell -ExecutionPolicy Bypass -File C:/trace/procmon-chkdsk-trace.ps1 -Image C:/trace/nfs.img -OutDir C:/trace/out'

# 4. Pull the results back.
mkdir -p /tmp/procmon-output
scp -i "$WIN_VM_KEY" "$WIN_VM_USER@$WIN_VM_HOST":C:/trace/out/'chkdsk-*' /tmp/procmon-output/
```

## What you get back

In `/tmp/procmon-output/`:

| File | Contents |
|---|---|
| `chkdsk-readonly.txt`         | Stdout of `chkdsk DRIVE:` (no /F). Should report no problems. |
| `chkdsk-readonly-exit.txt`    | The exit code. Expect 0. |
| `chkdsk-scan.txt`             | Stdout of `chkdsk DRIVE: /scan`. Should show "found problems that must be fixed offline". |
| `chkdsk-scan-exit.txt`        | The exit code. Expect 13. |
| `chkdsk-trace.etl`            | Raw ETW binary capture. Open in Windows Performance Analyzer (WPA) for the rich view. |
| `chkdsk-trace.csv`            | Full CSV export of every captured event. Large (50–100 MB typical). |
| `chkdsk-trace-summary.xml`    | `tracerpt` per-provider event summary. Useful for sanity. |
| `chkdsk-trace-filtered.csv`   | Rows mentioning chkdsk, the drive letter, or the volume's raw device path. The diagnostic file. |

## Reading `chkdsk-trace-filtered.csv`

The columns of interest:

- **Operation** — `ReadFile`, `CreateFile`, `QueryDirectory`,
  `QueryInformation`. Reads of `\\.\PhysicalDriveN` or
  `\Device\HarddiskVolumeN` at specific offsets are the raw NTFS
  structure reads we want to map back to our on-disk bytes.
- **Path** — the file or device handle.
- **Detail** — for `ReadFile`, includes "Offset: N, Length: M". For
  `CreateFile`, includes the desired access flags.

The events that `chkdsk` read-only ALSO does are uninteresting; the
*differential* events are the ones that only `/scan` does. Two ways
to surface them:

1. **Diff against a reference capture**: run the same script on a
   Microsoft `format.com`-formatted volume of identical scenario,
   then `diff` the two filtered CSVs by Operation + offset.
2. **Compare ranges between `chkdsk DRIVE:` (read-only) and
   `chkdsk DRIVE: /scan` within the same capture**: read-only's reads
   appear in the first half of the timeline, /scan's in the second.

Either way, the reads /scan does immediately before printing its
"must be fixed offline" line are the diagnostic gold — those offsets
point at the on-disk structure we're not getting right.

Map offsets back to our layout:

- Boot sector: offset 0 inside the partition.
- `$MFT` records: `mft_lcn * cluster_size + (record_no * mft_record_size)`.
- `$UpCase`: at `upcase_lcn * cluster_size` (currently LCN immediately
  after `$Bitmap`).
- `$Bitmap`: at `bitmap_lcn * cluster_size`.
- `$LogFile`: at `logfile_lcn * cluster_size`.

The actual values for a 256 MiB / 4 KiB-cluster volume are in
[`src/mkfs.rs`](../src/mkfs.rs)'s `format_filesystem` — search for
"LCN" comments.

## Reference comparison (the corroboration)

To compare against Microsoft `format.com`:

```powershell
# On the VM, format a reference volume identical to ours but with
# Microsoft's own formatter. Re-use the wrapper VHD dance from the
# script — easiest is to copy procmon-chkdsk-trace.ps1 and swap
# step 2's "write nfs.img" for "format with format.com after mount".
```

The CI workflow `.github/workflows/ci.yml` already has the reference-
formatter dance in the
`Build a reference Microsoft-formatted NTFS volume + diff against ours`
step — that's the source to crib from when you want a reference
capture for diffing.

## Iteration discipline

Mantra: "what does the diff say?" Don't change `src/mkfs.rs` from a
hypothesis. Capture the trace, identify the byte chkdsk keys on, cite
the public spec for that byte, change exactly that byte. Each
iteration writes to
[`docs/chkdsk-improvement-findings.md`](./chkdsk-improvement-findings.md)
so the methodology survives across sessions.
