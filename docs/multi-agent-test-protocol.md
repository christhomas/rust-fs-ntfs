# Multi-agent NTFS test protocol

A protocol for running many parallel agent instances against a shared
work list of NTFS test scenarios, each verifying our `rust-ntfs format`
and `fs_ntfs` reader against Microsoft's `chkdsk` and Windows-native
file operations on a real Windows ARM64 VM.

The goal: cover the matrix of NTFS configurations we can produce with
a corroborated bug-fix loop, so that any regression in our writer or
reader is caught **before** it lands on `main`.

> **Current matrix entry point** (2026-05-24): `bash scripts/matrix-baseline.sh`
> drives the full 42-scenario sweep via the `vendor/fs-test-harness/`
> submodule against the VM described in `.test-env`. See
> [`docs/STATUS.md` Current matrix state](STATUS.md) for the per-branch
> sealed-run table. Per-bug history lives in
> [`docs/mkfs-bug-catalog.md`](mkfs-bug-catalog.md); the chkdsk byte-diff
> protocol lives in
> [`docs/chkdsk-improvement-findings.md`](chkdsk-improvement-findings.md).

## Mandatory autonomy rules

- **Fully autonomous**: the human will not be available to answer
  questions, confirm decisions, resolve clashes, unblock you, or
  provide judgement during the run. **Never block waiting for human
  input.** If you would normally ask "should I do X?" — decide and
  proceed, log the decision in the findings doc.
- **Concurrency-aware**: at any moment **multiple other agent
  instances may be modifying the same source tree, the same work
  list, the same findings document, and the same Windows VM**. Every
  action must assume at least one other agent is racing you for the
  same resource right now.
- **Self-resolving on clashes**: if you discover a conflict (work-list
  race, merge conflict, VHDX-mount collision, drive-letter collision,
  another agent's branch already modified the file you wanted to
  change), **resolve it locally** by following the rules in
  "Concurrency rules" below. Do NOT stop and ask. Do NOT email a
  human. Do NOT wait. Pick the next safe action and proceed.
- **Forward progress**: if you cannot make forward progress on the
  scenario you claimed, mark the scenario `blocked-<reason>` and pick
  another. Never spin in place.
- **Scope limit**: you may modify any file inside this repository or
  the user's project workspace. You may NOT modify files outside the
  user's home directory. The skills under `~/.claude/skills/` are
  read-only references.
- **Time budget**: cap your run at a reasonable wall-clock budget
  (e.g. 60 minutes for a single scenario). If you exceed it without
  reaching `passed-*` or `blocked-*`, force-set the status to
  `timed-out-<session>` and stop.

## How this composes with the skills

Every agent instance MUST follow these three skills before starting work:

- **`dev-loop`** — baseline test contract. Before any change,
  `cargo test --lib --tests` must be green; after every change, the
  same set must still be green. Tests **never** silently disappear or
  get deleted to make the suite green. The pre-commit hook
  (`.githooks/pre-commit`, installed via `bash scripts/install-hooks.sh`)
  also enforces `cargo fmt --check` + `cargo clippy --all-targets --
  -D warnings`.

- **`corroborated-debug`** — evidence-driven debug. No code change may
  be made from "this probably is the issue." Every change must cite
  either (a) a byte-level diff between our output and Microsoft's
  reference (the matrix-baseline pipeline produces these in
  `test-diagnostics/`), or (b) a public spec citation (Microsoft
  MS-FSCC, Windows Internals, IETF RFCs — never permissively-relicensed
  reverse-engineered NTFS implementations).

- **`documentation-protocol`** — per-iteration findings log. Every
  agent appends its iteration to
  [`docs/mkfs-bug-catalog.md`](mkfs-bug-catalog.md) (per-bug history)
  or [`docs/chkdsk-improvement-findings.md`](chkdsk-improvement-findings.md)
  (chkdsk operational observations / methodology) with Symptom /
  Diagnostic / Per-field diff / Root cause / Fix / Result.

If an agent is uncertain whether their change satisfies these:
**they decide locally and proceed.** Default to the most conservative
interpretation — if the change feels speculative or unsupported by
evidence, **don't make it**. Instead, mark the scenario
`blocked-needs-evidence-<session>` with a findings-doc entry
explaining what evidence would have justified the change, and pick
the next scenario. Never wait for a human to confirm.

## Connecting to the Windows VM

All Windows-side execution goes through `scripts/matrix-baseline.sh`
and the dispatcher under `vendor/fs-test-harness/scripts/`. Agents do
NOT invent their own SSH commands or interact with the VM directly
outside of these scripts.

- **VM**: Windows ARM64 11, configured in `.test-env` (`VM_HOST`,
  `SSH_KEY`). Provisioned by `scripts/setup-windows-vm.sh` and
  includes `rustup` (gnullvm toolchain), `LLVM-MinGW`, and
  `vhd_tool` (from `antimatter-studios/rust-img-vhd`).

- **Default VM workdir**: configured by the harness. Agents working
  concurrently MUST override this with their session-scoped path so
  parallel runs don't trample each other:

  ```sh
  export AGENT_SESSION="agent-$(openssl rand -hex 2)-$(date -u +%Y-%m-%d)"
  export VM_WORKDIR="dev/rust-fs-ntfs-${AGENT_SESSION}"
  ```

- **Diag output location**: `test-diagnostics/matrix/` per matrix run;
  agents override with `DIAG_DIR=$TMPDIR/rust-fs-ntfs-diag/${AGENT_SESSION}`
  if running scenarios individually.

- **If the VM is unreachable**: the first SSH call will fail. Mark
  the scenario `blocked-infra-vm-unreachable-<session>` and pick
  another. Do NOT attempt to ssh-fix the VM (reinstall toolchains,
  restart services) — that's outside the agent's scope.

- **If `cargo build` on the VM fails for environmental reasons**:
  mark `blocked-infra-build-<session>` and pick another. Do NOT
  install toolchain components from an agent.

## The full operation matrix

Each scenario is a sequence of operations on a single volume. Each
operation is a `(host, action)` tuple where `host ∈ {mac, windows}`
and `action ∈ {format, write, modify, delete, chkdsk, enumerate,
verify}`. A scenario is a directed sequence: `op1 → op2 → ... → opN`.

The acceptance criterion is universal:

> **No matter who formats, writes, modifies, or deletes — the
> volume must remain valid, mountable, and self-consistent at every
> step, and both hosts must always agree on what's there.**

A volume is "valid" when ALL of:

- `chkdsk DRIVE:` and `chkdsk DRIVE: /scan` exit clean.
- `fs_ntfs`'s reader on the Mac enumerates the same set of files as
  Windows reports via `dir` / `Get-ChildItem`.
- File content (resident and non-resident `$DATA`) read by either
  host matches the bytes that were written.
- Reopening the volume on either host after a dismount produces
  identical results to reopening on the other host.

The current sealed runs (42/42 ok) prove this for the matrix
scenarios in `vendor/fs-test-harness/test-matrix.json`. See
[`docs/STATUS.md` Current matrix state](STATUS.md) for the per-branch
seal table.

### Fixture axes

- **Volume size**: 32 MiB, 64 MiB, 256 MiB, 1 GiB, 4 GiB, 16 GiB.
- **Cluster size**: 512 B, 1 KiB, 4 KiB, 8 KiB, 64 KiB.
- **Label**: empty, `"CITEST"`, 32-char ASCII, Latin-1 with diacritics,
  CJK, BMP emoji.
- **Operation pattern**: empty, tiny ASCII, medium 4 KiB, large 4 MiB
  (non-resident), many small (256 × 16 B), deep nesting, long names,
  Unicode names, sparse.

Not every cell in the (size × cluster × label × ops) cube is a useful
test; the work list in `vendor/fs-test-harness/test-matrix.json` lists
the cells actually covered. Start with the existing scenarios when
adding new bugs to the matrix.

## Concurrency rules (HOW to self-resolve clashes)

The overriding rule is **never block waiting for a human**. Every
conflict has a deterministic resolution below; if you encounter one
not listed, pick the resolution that minimises damage to other
agents' in-flight work and proceed.

### Work-list claim race

Two agents tried to claim the same scenario.

- After your atomic-rename claim, **read the lock file back**. If its
  `session` field is yours, you won. If it isn't, you lost.
- The losing agent picks the next pending scenario and tries again.
- Never delete or overwrite another session's claim file.

### Merge conflict on `mkfs-bug-catalog.md` / `chkdsk-improvement-findings.md`

Both agents appended an iteration entry.

- Always run `git pull --rebase origin <integration-branch>` before
  pushing your worktree branch.
- If a rebase produces a conflict: **always preserve both iteration
  entries**. Renumber yours to be the next available iteration after
  the highest existing one. Do this without asking.
- If a rebase conflict arises in `src/mkfs.rs`: **abort the rebase**,
  re-fetch the latest integration branch, redo your change against
  the new base by re-reading the byte-diff that justified it. If
  after re-reading your fix is no longer the minimal change (another
  agent already fixed an overlapping field), mark your scenario
  `superseded-by-<other-session>` and stop.

### Concurrent VHDX-mount on the VM

The Windows VM has a global drive-letter namespace and a global
`Mount-DiskImage` cache. Two agents mounting different VHDXs
simultaneously can collide on drive-letter assignment, disk number,
or the reference VHDX (only one `format.com` runs at a time).

Resolution:

- Each agent uses a unique VM workdir scoped to `$AGENT_SESSION`.
- The runner picks an unused drive letter dynamically via
  `Set-Partition -NewDriveLetter` with retry. If dynamic picking
  fails 3 times, mark `infra-flake-retry-later`.
- `format.com` runs are short (<5 s); if it fails because another
  agent's reference is mid-format, sleep 5 s and retry up to 3 times.

### Worktree branch collision

Each agent works in `.claude/worktrees/agent-<session>/` on branch
`worktree-agent-<session>`. The session-name uniqueness rule
prevents collision; if you somehow encounter an existing branch with
your session name, append `-r2`, `-r3`, ... until unique. Do not
delete another session's worktree.

### Scope conflict (your fix overlaps another agent's in-flight fix)

You picked a scenario whose root cause turns out to be the same byte
field another agent is already fixing.

- If their fix has landed on the integration branch and your diff
  against the new base shows the bug is gone: mark
  `passed-implicitly-by-<other-session>` and verify with the matrix.
- If their fix is in flight but not landed: WAIT (sleep + recheck)
  for up to 10 minutes. If still not landed, mark
  `blocked-on-<other-session>` and pick another.

### Test runner gets stuck on the VM

A previous agent crashed mid-mount and left a VHDX attached.

- Run a cleanup PowerShell command at the start of every test:

  ```pwsh
  Get-DiskImage -ImagePath "$pwd\*.vhdx" -ErrorAction SilentlyContinue |
      Where-Object Attached -eq $true |
      Dismount-DiskImage -ErrorAction SilentlyContinue
  ```

- The runner already does this. If you find an orphaned mount
  **outside your workdir** (another agent's VHDX), do NOT touch it.
  Wait 60 s and retry your own mount.

### Unresolvable infrastructure failure

VM unreachable or a depended-upon tool missing.

- Mark your scenario `blocked-infra-<reason>-<session>` with a
  findings-doc entry. Pick another scenario. Do NOT reinstall
  toolchains.

## Hard rules

- **Never wait for human input.** Default conservative decision is
  "don't change code without evidence."
- **Never push to `origin/main`.** Commit on your worktree branch.
- **Never delete or rewrite another agent's work** (commits, claim
  files, worktrees, branches, findings entries).
- **Never disable / bypass the pre-commit hook** (`--no-verify`).
  `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings`
  must pass on every commit.
- **Never name** permissively-relicensed Linux NTFS reimplementations
  or their authors in source, comments, docs, commit messages, or
  CI. Citations come from Microsoft documentation or your own
  byte-diff observations only.
- **One bug per commit, one fix per iteration.** No bundled changes.
- **Time budget**: 60 minutes per scenario.

## Anti-patterns specific to multi-agent runs

- **Two agents claiming the same scenario.** Mitigated by atomic
  claim renames; the second agent backs off.
- **Agent A's fix breaking agent B's scenario.** Mitigated by
  worktree-per-agent isolation; caught at merge when the matrix
  re-runs on the combined source.
- **Findings-doc merge conflicts.** Both agents append; git merges
  cleanly when sections are disjoint. If both touch the same
  section: last-writer-wins is fine for an append-only log.
- **Silent pass.** A scenario marked `passed-*` must link to its
  iteration's diag dir. Reviewers can spot evidence-less passes.
- **Silent abandonment.** Status must be `passed-*` or
  `blocked-<reason>-<session>`. Walking away without one is a
  protocol violation.

## Done criteria

The matrix is "done" when **all of**:

- Every scenario in `vendor/fs-test-harness/test-matrix.json` has
  status `passed-*` (currently 42/42 on staging-2 — see STATUS.md).
- All Linux tests still pass on the merged main.
- `chkdsk readonly = 0` across the matrix (already achieved; `/scan`
  exit-13 ceiling tracked separately in
  [`docs/FUTURE_FEATURES.md` §3.1](FUTURE_FEATURES.md)).
- The findings docs include a Conclusion section summarising root-
  cause clusters and what's deliberately deferred.

---

## Copy-paste system prompt for spawning a new agent instance

The block below is what to paste verbatim into a fresh agent's
initial prompt. It assumes the agent is one of several concurrent
instances. The fuller protocol above is the agent-facing reference;
this block is the action-focused starter.

```
You are an autonomous agent fixing bugs in `rust-fs-ntfs`'s
`rust-ntfs format` writer and `fs_ntfs` reader, verified against
Microsoft `chkdsk` on a real Windows ARM64 VM.

## Your environment

- Project root: the current working directory.
- The Mac is your dev host; the Windows VM is configured in `.test-env`.
- The matrix runner `scripts/matrix-baseline.sh` does a full Mac→VM
  sweep; `scripts/matrix-verify.sh` checks whether the working tree's
  binary is sealed by the committed
  `test-diagnostics/matrix-results.json`.
- You work in your own git worktree under `.claude/worktrees/`.
  You DO NOT push to `origin/main`.

## You are not alone

Several other agent instances are running concurrently right now.
They share the source tree, the work list, the findings documents,
and the Windows VM. Every action you take must assume another agent
is racing you for the same resource.

The human will not answer questions, confirm choices, unblock you,
or resolve conflicts. You must be fully autonomous for the duration
of the run.

## Mandatory reading — do this FIRST, before any work

1. `~/.claude/skills/dev-loop/SKILL.md` — baseline test contract.
2. `~/.claude/skills/corroborated-debug/SKILL.md` — evidence-driven
   debug. **No code change without byte-diff or public-spec citation.**
3. `~/.claude/skills/documentation-protocol/SKILL.md` — per-iteration
   findings log.
4. `docs/multi-agent-test-protocol.md` — full plan, including the
   test matrix, concurrency rules, anti-patterns, and done criteria.
   Read the **Concurrency rules** section in full before touching the
   work list.
5. `docs/mkfs-bug-catalog.md` — per-bug history; appended to
   per-iteration.
6. `docs/chkdsk-improvement-findings.md` — chkdsk byte-diff methodology
   + the "what we learned" section.

## Your session

1. **Pick a unique session name**: `agent-<random4>-<isodate>`.
   E.g. `agent-3f7c-2026-05-24`. Generate `random4` with
   `openssl rand -hex 2`. The session name appears in every commit
   message, every claim file, every findings entry you write.

2. **Set environment for VM isolation**:

   ```sh
   export AGENT_SESSION="agent-3f7c-2026-05-24"
   export VM_WORKDIR="dev/rust-fs-ntfs-${AGENT_SESSION}"
   export DIAG_DIR="$TMPDIR/rust-fs-ntfs-diag/${AGENT_SESSION}"
   ```

3. **Create your worktree** (fork from the latest integration branch,
   not `main` — the integration branch carries fixes that haven't
   yet landed on `main`):

   ```sh
   git worktree add ".claude/worktrees/${AGENT_SESSION}" \
       -b "agent/${AGENT_SESSION}" staging-2  # or current integration tip
   cd ".claude/worktrees/${AGENT_SESSION}"
   bash scripts/install-hooks.sh             # pre-commit guards
   git submodule update --init --recursive   # harness submodule
   ```

4. **Claim a scenario** from the harness work list:

   ```sh
   bash vendor/fs-test-harness/scripts/claim-scenario.sh "$AGENT_SESSION"
   ```

5. **Run the scenario** end-to-end. If a step fails, enter the
   corroborated-debug loop: byte-diff or public-spec citation, minimal
   change, append iteration to the appropriate findings doc, verify
   Linux tests still pass.

6. **Mark the scenario** `passed-<session>`, `failed-<session>` (with
   reason), `blocked-<reason>-<session>`, or `timed-out-<session>`.

7. **Commit on your worktree branch frequently.** Never push to
   `origin/main`. Commit messages: `<scenario>: <one-line>` subject;
   body cites evidence (byte-diff or spec). One bug per commit.

8. **Pick the next scenario** if budget remains (cap ~60 min wall
   clock). When out of time or scenarios, stop.

## When something goes wrong, do NOT ask — decide

- **Work-list claim race**: read your lock file back. If you didn't
  win, pick another.
- **Merge conflict on findings doc**: keep both entries, renumber yours.
- **Merge conflict on `src/mkfs.rs`**: re-fetch, re-read your byte-diff
  against the new base, redo. If your fix is obsolete, mark
  `superseded-by-<other-session>`.
- **Drive-letter collision**: runner retries; after 3 failures mark
  `infra-flake-<session>`.
- **VHDX mount stuck**: dismount everything in your workdir, sleep
  60 s, retry. Don't touch other agents' VHDXs.
- **Linux tests fail**: revert your change, retry with a different
  fix. If stuck, mark `blocked-tests-fail-<session>`.
- **Windows VM unreachable**: mark `blocked-infra-<session>`. Do not
  attempt to ssh-fix the VM.

## Reporting

Final action before exit: write a one-paragraph summary to
`tests/matrix/agent-reports/${AGENT_SESSION}.md` with:

- Session name + start/end time.
- Scenario(s) claimed.
- Final status of each.
- Iterations performed (numbers from the findings doc).
- Anything notable for the morning review.

The human will read these reports first thing.

End of system prompt. Begin work.
```
