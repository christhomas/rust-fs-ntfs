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
- `qemu-img.exe` on PATH (`winget install cloudbase.qemu-img`).
- Administrator (the script's VHDX mount + raw `\\.\PhysicalDrive`
  write + Procmon driver-load all need elevation).

The script auto-downloads `Procmon64.exe` from
`live.sysinternals.com` on first run.

## End-to-end flow

From the build machine (typically the dev Mac / Linux box where this
repo lives):

```bash
# 1. Build rust-ntfs locally and produce an nfs.img to send to the VM.
cargo build --release --bin rust-ntfs
mkdir -p /tmp/procmon-input
./target/release/rust-ntfs format -L CITEST --serial deadbeefcafe1234 \
    --create-size 256M /tmp/procmon-input/nfs.img

# 2. Copy the image + the trace script to the VM.
#    Adjust the user/host/key to match your VM.
VM=chris@192.168.213.147
KEY=/path/to/vm/privatekey
scp -i "$KEY" /tmp/procmon-input/nfs.img \
              scripts/procmon-chkdsk-trace.ps1 \
    "$VM":C:/trace/

# 3. Run the trace.
ssh -i "$KEY" "$VM" \
    'powershell -ExecutionPolicy Bypass -File C:/trace/procmon-chkdsk-trace.ps1 -Image C:/trace/nfs.img -OutDir C:/trace/out'

# 4. Pull the results back.
scp -i "$KEY" "$VM":C:/trace/out/'chkdsk-*' /tmp/procmon-output/
```

## What you get back

In `/tmp/procmon-output/`:

| File | Contents |
|---|---|
| `chkdsk-readonly.txt`        | Stdout of `chkdsk DRIVE:` (no /F). Should report no problems. |
| `chkdsk-readonly-exit.txt`   | The exit code. Expect 0. |
| `chkdsk-scan.txt`            | Stdout of `chkdsk DRIVE: /scan`. Should show "found problems that must be fixed offline". |
| `chkdsk-scan-exit.txt`       | The exit code. Expect 13. |
| `chkdsk-trace.pml`           | Raw Procmon binary capture (open in Procmon GUI for the rich view). |
| `chkdsk-trace-all.csv`       | Full CSV export of every captured event. Large. |
| `chkdsk-trace-filtered.csv`  | `chkdsk.exe`-only events on the mounted drive. The diagnostic file. |

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
# Microsoft's own formatter. Re-use the wrapper VHDX dance from the
# script — easiest is to copy procmon-chkdsk-trace.ps1 and swap
# step 2's "write nfs.img" for "format with format.com after mount".
```

The CI workflow `.github/workflows/ci.yml` already has the reference-
formatter dance in the
`Build a reference Microsoft-formatted NTFS volume + diff against ours`
step — that's the source to crib from when you want a reference
capture for diffing.

## Iteration discipline

Per
[`/Users/christhomas/.claude/skills/corroborated-debug/SKILL.md`](../../.claude/skills/corroborated-debug/SKILL.md)
"Mantra: What does the diff say?". Don't change `src/mkfs.rs` from a
hypothesis. Capture the trace, identify the byte chkdsk keys on, cite
the public spec for that byte, change exactly that byte. Each
iteration writes to `docs/chkdsk-improvement-findings.md` so the
methodology survives.
