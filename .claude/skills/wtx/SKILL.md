---
name: Windows Test Matrix (wtx)
description: |
  The discipline for testing NTFS write/read changes against a Windows VM
  using the 42-scenario test matrix. Covers tiered gates (cargo test →
  smoke matrix → full matrix), the matrix-results.json seal pattern,
  parallel development via worktrees, and the staging-branch integration
  workflow. Invoke when: starting a new feature branch, sealing a PR,
  diagnosing a matrix regression, or onboarding to the workflow.
short_name: wtx
---

# Windows Test Matrix (wtx) — discipline

This skill is the operating manual for the `rust-fs-ntfs` test matrix:
how to run it, how to seal results into git commits, how to develop
multiple features in parallel without blocking on 3–4-hour matrix
runs, and how to diagnose regressions cheaply.

## The matrix in one paragraph

`bash scripts/run-matrix.sh` runs 42 scenarios against a Windows ARM64
VM. Each scenario formats an NTFS volume (mac side), ships it to the
VM, mounts + exercises + chkdsk-scans it, fetches diagnostics back.
Total wall-clock is ~3–4 hours on the current single-threaded harness.
The matrix is the **authoritative** signal that an NTFS code change
hasn't broken anything Windows cares about — but it is too expensive
to run on every commit, so the rest of this doc is about *when* to run
it.

## Tiered gates — cheapest first

Failing earlier is always cheaper. Always run cheaper gates first.

| Gate | Cost | What it catches | When to run |
|---|---|---|---|
| **1. `cargo test --release`** | seconds | obvious logic + per-module bugs | every code change |
| **2. Smoke matrix** (`bash scripts/matrix-baseline.sh --smoke`) | ~15 min | branch-local NTFS regressions | after a logical feature lands |
| **3. Sequential rebase onto staging** | seconds–minutes | textual conflicts between branches | when integrating multiple features |
| **4. Full matrix** (`bash scripts/matrix-baseline.sh`) | ~3–4 hr | semantic regressions, feature interactions | once per PR, at branch tip |
| **5. Rebase main onto staging** | seconds | none (promotion only) | after staging matrix is green |

The smoke set (5 scenarios) is chosen to cover the most-distinct code
paths cheaply:

- `mac-format-basic-256mib` — fresh-format clean-volume baseline
- `mac-format-tiny-32mib` — small-volume edge case
- `mac-format-mkdir-set-dirty-win-chkdsk` — runtime mkdir + chkdsk
- `mac-format-mac-write-win-repeat-mount-3-win-chkdsk` — repeated-mount path
- `mac-format-win-write-many-win-delete-half-win-chkdsk` — Win write + Win delete

## The seal — matrix-results.json

Every matrix run writes `test-diagnostics/matrix-results.json`. The
file contains, alongside per-scenario verdicts:

- `tested_at_sha` — git HEAD when the run started.
- `binary_sha256` — sha256 of `target/release/rust-ntfs` at run time.
  **This is the load-bearing seal**: content-addressable, survives
  rebase / squash-merge / cherry-pick.
- `harness_submodule_sha` + `test_matrix_json_sha256` — pin the runner
  and the scenario definitions.
- VM metadata: Windows build, ntfs.sys version, chkdsk version. Lets
  future readers diagnose multi-version validator drift.

A commit is **sealed** when its tree contains a `matrix-results.json`
whose `binary_sha256` matches a fresh `cargo build --release` of that
tree.

Check the seal at any time:

```bash
bash scripts/matrix-verify.sh           # exits 0 iff sealed
bash scripts/matrix-verify.sh --build   # rebuild first if binary stale
```

## The discipline — worktrees + staging

The matrix is a serial gate (one VM at a time). The cure for "matrix
takes 3 hours and blocks me" is to keep working on a different branch
in a different worktree while the matrix grinds.

### Per-feature workflow

```bash
# Spawn a worktree for the feature
git worktree add ../<repo>-<feature> -b feature/<feature> main

# Develop freely; rely on cargo test for inner-loop confidence
cd ../<repo>-<feature>
# ... edit, commit ...
cargo test --release

# Optional sanity gate before integration
bash scripts/matrix-baseline.sh --smoke

# When code-complete, return to main repo for integration (see below)
```

### Integrating multiple features via staging

```bash
git worktree add ../<repo>-staging -b staging main
cd ../<repo>-staging

# Cheap-fail gate: sequential rebase finds textual conflicts before any matrix cost
git rebase feature/s4-reparse
git rebase feature/s5-txflog
git rebase feature/obj-write

# When all branches integrate textually, run the full matrix
bash scripts/matrix-baseline.sh

# Diff against the inherited baseline (main's matrix-results.json was
# the starting point of staging's tree)
bash scripts/matrix-diff.sh \
    /path/to/main-worktree/test-diagnostics/matrix-results.json \
    test-diagnostics/matrix-results.json

# If green → commit the new matrix-results.json onto staging
git add test-diagnostics/matrix-results.json
git commit -m "chore: matrix verified @ $(git rev-parse --short HEAD)"

# Promote to main
git checkout main
git rebase staging
git push origin main
```

### Why this works

| Property | Why it matters |
|---|---|
| Only `main` carries an authoritative seal | Single source of truth; per-branch matrix runs are optional |
| Branches inherit the baseline via git | Zero copy-around; `matrix-results.json` is just a file in the tree |
| Full matrix runs only at PR-prep / staging | Amortises the 3-hour cost over an entire integration batch |
| `binary_sha256` is the seal | Survives rebase / squash-merge; commit-SHA churn doesn't invalidate the verification |
| Sequential rebase before matrix | Catches branch-vs-branch conflicts in seconds instead of hours |

## Diagnosing a matrix regression

When `scripts/matrix-diff.sh` reports `REGRESSIONS`, the bisect-and-fix
loop is:

1. **Identify the scenario.** `matrix-diff.sh` names it; the per-step
   diag artifacts live in `test-diagnostics/matrix/<scenario>/step-NN/`
   on the orchestrator. The per-scenario `verdict.json` from the VM
   has chkdsk exit codes per lane (readonly / scan / fix / post-fix scan).
2. **Pull the chkdsk stdout** from the VM for the failing scenario:
   ```bash
   ssh "$VM_HOST" "powershell -Command \"Get-Content $VM_WORKDIR/diag/v2/<scenario>/chkdsk-readonly.txt\""
   ```
   This usually names a specific MFT segment, attribute, or index entry
   that chkdsk flagged.
3. **Bisect with smoke matrix** if the regression could be in any of
   several commits in a stack. `git bisect` between the known-good
   commit (the baseline's `tested_at_sha`) and HEAD; at each bisect
   step run `bash scripts/matrix-baseline.sh --smoke` (15 min, not 3
   hr). For a 5-commit stack that's ~45 minutes total to identify the
   culprit.
4. **Reproduce in isolation**: `bash scripts/run-matrix.sh <scenario>`
   runs just the one scenario (~5 min for fast scenarios, up to 8 min
   for write-heavy ones).

## Reference: the script set

| Script | What it does |
|---|---|
| `scripts/run-matrix.sh` | Underlying harness wrapper (`vendor/fs-test-harness`). Filters by substring. |
| `scripts/matrix-baseline.sh` | Runs matrix + writes `test-diagnostics/matrix-results.json`. `--smoke` for 5 scenarios. |
| `scripts/matrix-diff.sh OLD NEW` | Pretty deltas. Non-zero exit on `ok → FAILED` regressions. |
| `scripts/matrix-verify.sh` | Check seal status of working tree against committed JSON. |
| `scripts/win/vm-info.ps1` | Emits VM metadata JSON (over SSH). |
| `scripts/win/verdict-collect.ps1` | Aggregates per-scenario `verdict.json` (over SSH). |
| `scripts/_matrix-collect-vm.sh` | Internal: SSH + run PS scripts + invoke the Python builder. |
| `scripts/_matrix-build-json.py` | Internal: combines metadata + verdicts into `matrix-results.json`. |

## Quick reference — common operations

**Run the full matrix and seal HEAD:**
```bash
bash scripts/matrix-baseline.sh && \
git add test-diagnostics/matrix-results.json && \
git commit -m "chore: matrix verified @ $(git rev-parse --short HEAD)"
```

**Smoke-only sanity check on a feature branch:**
```bash
bash scripts/matrix-baseline.sh --smoke
```

**Inspect what scenarios are still hitting `/scan` exit 13:**
```bash
python3 -c "
import json; m=json.load(open('test-diagnostics/matrix-results.json'))
for n,s in m['scenarios'].items():
    e = (s.get('exits') or {}).get('/scan')
    if e and e != 0: print(f'{n}: /scan={e}')"
```

**Compare two branches' seals:**
```bash
git show main:test-diagnostics/matrix-results.json > /tmp/m1.json
git show staging:test-diagnostics/matrix-results.json > /tmp/m2.json
bash scripts/matrix-diff.sh /tmp/m1.json /tmp/m2.json
```

## What this skill explicitly does NOT cover

- The Windows VM provisioning (IP, SSH keys, work directory) — that's
  in `vendor/fs-test-harness/scripts/setup-local.sh` and the
  `.test-env` file.
- The harness internals — see `vendor/fs-test-harness/README.md`.
- The clean-room reverse-engineering rules — see
  `docs/chkdsk-improvement-findings.md` §1.6.

## Known-future improvements (not yet shipped)

- **Parallel matrix execution** on the VM (currently `--test-threads=1`).
  Each scenario can use its own drive letter; the only critical section
  is the mount step. Estimated 3–4× wall-clock speedup once wired.
  See future-features.md.
- **Pre-push hook** refusing pushes to `main` when the working tree is
  unsealed (verified via `matrix-verify.sh`).
- **CI auto-seal**: on matrix pass, CI commits the updated JSON back to
  the branch as a `chore:` commit so PR diffs always show the
  verification record.