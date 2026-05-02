# Multi-agent NTFS test protocol

A protocol for running many parallel agent instances against a shared
work list of NTFS test scenarios, each verifying our `mkfs_ntfs` and
`fs_ntfs` reader against Microsoft's `chkdsk` and Windows-native file
operations on a real Windows ARM64 VM.

The goal: cover the matrix of NTFS configurations we can produce with
a corroborated bug-fix loop, so that any regression in our writer or
reader is caught **before** it lands on `main`.

## Mandatory autonomy rules (read first)

- **Fully autonomous**: the human will not be available to answer
  questions, confirm decisions, resolve clashes, unblock you, or
  provide judgement during the run. **Never block waiting for human
  input.** If you would normally ask "should I do X?" -- decide and
  proceed, log the decision in the findings doc.
- **Concurrency-aware**: at any moment **multiple other agent
  instances may be modifying the same source tree, the same work
  list, the same findings document, and the same Windows VM**. Every
  action must assume at least one other agent is racing you for the
  same resource right now.
- **Self-resolving on clashes**: if you discover a conflict (work-list
  race, merge conflict, VHDX-mount collision, drive-letter collision,
  another agent's branch already modified the file you wanted to
  change, your scenario was claimed by someone else between the
  moment you decided to claim it and the moment you wrote the lock
  file...), **resolve it locally** by following the rules in
  "Concurrency rules" below. Do NOT stop and ask. Do NOT email a
  human. Do NOT wait. Pick the next safe action and proceed.
- **Forward progress**: if you cannot make forward progress on the
  scenario you claimed (e.g. dependency on another agent's in-flight
  fix, infrastructure not yet scaffolded, etc.), mark the scenario
  `blocked-<reason>` and pick another. Never spin in place.
- **Scope limit**: you may modify any file inside this repository or
  the user's project workspace. You may NOT modify files outside the
  user's home directory. The skills under `~/.claude/skills/` are
  read-only references.
- **Time budget**: cap your run at a reasonable wall-clock budget
  (e.g. 60 minutes for a single scenario). If you exceed it without
  reaching `passed-*` or `blocked-*`, force-set the status to
  `timed-out-<session>` and stop. Don't burn the night chasing one
  scenario.

## How this composes with our skills

Every agent instance MUST follow these three skills (read each before
starting any work):

- **`dev-loop`** -- baseline test contract. Before any change, every
  Linux test in `cargo test --release --lib --test mkfs_roundtrip
  --test mkfs_bin_smoke` must pass; after every change, the same set
  must still pass. Tests appearing only after a change must also pass.
  Tests **never** silently disappear or get deleted to make the suite
  green.

- **`corroborated-debug`** -- evidence-driven debug. No code change
  may be made from "this probably is the issue." Every change must
  cite either (a) a byte-level diff between our output and Microsoft's
  reference, or (b) a public spec citation (Microsoft MS-FSCC,
  Windows Internals, IETF RFCs -- never GPL'd reverse-engineered NTFS
  implementations). The local pipeline (`scripts/test-windows-local.sh`)
  is the corroboration mechanism.

- **`documentation-protocol`** -- per-iteration findings log. Every
  agent appends its iteration to `docs/chkdsk-findings.md` with
  Symptom / Diagnostic / Per-field diff / Root cause / Fix / Result.
  The findings doc is part of the deliverable.

If an agent is uncertain whether their change satisfies these:
**they decide locally and proceed.** Default to the most conservative
interpretation -- if the change feels speculative or unsupported by
evidence, **don't make it**. Instead, mark the scenario
`blocked-needs-evidence-<session>` with a findings-doc entry
explaining what evidence would have justified the change, and pick
the next scenario. Never wait for a human to confirm. Never merge
speculative changes -- when in doubt, don't change.

## The full operation matrix

Each scenario is a sequence of operations on a single volume. Each
operation is a (host, action) tuple where `host ∈ {mac, windows}` and
`action ∈ {format, write, modify, delete, chkdsk, enumerate, verify}`.
A scenario is a directed sequence: `op1 -> op2 -> ... -> opN`.

The acceptance criterion for a scenario is universal:

> **No matter who formats, writes, modifies, or deletes -- the
> volume must remain valid, mountable, and self-consistent at every
> step, and both hosts must always agree on what's there.**

A volume is "valid" when ALL of:

- `chkdsk DRIVE:` and `chkdsk DRIVE: /scan` exit clean (no errors,
  not just no fatals).
- `fs_ntfs`'s reader on the Mac enumerates the same set of files as
  Windows reports via `dir` / `Get-ChildItem`.
- File content (resident and non-resident `$DATA`) read by either
  host matches the bytes that were written.
- Reopening the volume on either host after a dismount produces
  identical results to reopening on the other host.

### Operation sequences (combinations the matrix covers)

Some illustrative sequences -- the work list contains the full set:

| # | Sequence (-> = dismount + reopen on next host) |
|---|---|
| 1 | `mac:format -> win:chkdsk -> win:write(F1,F2) -> win:chkdsk -> mac:enumerate(F1,F2)` |
| 2 | `mac:format -> mac:write(F1,F2) -> win:chkdsk -> win:enumerate(F1,F2)` |
| 3 | `win:format -> win:write(F1) -> mac:enumerate(F1) -> mac:write(F2) -> win:chkdsk -> win:enumerate(F1,F2)` |
| 4 | `mac:format -> win:write(F1) -> win:delete(F1) -> mac:enumerate(empty) -> win:chkdsk` |
| 5 | `win:format -> win:write(F1,F2,F3) -> mac:delete(F2) -> win:chkdsk -> win:enumerate(F1,F3)` |
| 6 | `mac:format -> win:write(F1) -> win:modify(F1) -> mac:enumerate(F1) -> mac:read(F1) == new bytes` |
| 7 | `win:format -> win:write(F1) -> mac:write(F2) -> win:write(F3) -> mac:enumerate(F1,F2,F3)` |
| 8 | `win:format -> win:write(many small files) -> mac:enumerate(all) -> win:delete(half) -> mac:enumerate(half)` |

The matrix combines: (which host formats) × (which sequence of
write/modify/delete operations from each side) × (which fixture set
of files is used) × (volume parameters).

### Operation host requirements

| Host | What it can do today | What it needs |
|---|---|---|
| Mac: format | `mkfs_ntfs` CLI | already works |
| Win: format | `format.com /FS:NTFS` | already works |
| Mac: write | NOT YET -- needs writer plumbed in `fs_ntfs` | first agent that picks a Mac-write scenario scaffolds it |
| Win: write | PowerShell file ops on mounted drive letter | already works |
| Mac: enumerate | `fs_ntfs` reader (existing) -- but no CLI binary yet | first agent that picks any scenario that needs enumerate scaffolds the CLI |
| Win: enumerate | `Get-ChildItem -Recurse` | already works |
| Mac: delete | NOT YET -- needs deleter | first agent that picks a Mac-delete scenario scaffolds it |
| Win: delete | `Remove-Item` | already works |
| Mac: chkdsk-equivalent | not applicable -- we don't have a Mac NTFS verifier | use `fs_ntfs` enumerate as the Mac-side check |
| Win: chkdsk | `chkdsk DRIVE:` and `chkdsk DRIVE: /scan` | already works |

The "NOT YET" items are scope. The first agent that picks a scenario
requiring them treats scaffolding as part of their iteration's task,
under the same skills discipline. If the scaffolding turns out to be
larger than one iteration's worth (more than ~500 lines of new
production code), the agent marks the scenario `blocked-on-writer`
or `blocked-on-deleter` and picks another scenario, leaving a
findings-doc entry naming the missing capability so a later agent
can attempt it.

### Why this matrix proves correctness

The acceptance criterion is symmetric: the volume is valid no matter
who edited it, in any order. If our writer is wrong in a way Windows
tolerates but our reader doesn't, the matrix catches it (Mac writes,
Windows can't read, our reader can). If our reader misses files
Windows wrote, the matrix catches it (Win writes, Mac enumerate
disagrees with Win Get-ChildItem). If our delete leaves dangling
state Windows can't recover from, the matrix catches it
(Mac deletes, Win chkdsk reports corruption).

Each scenario passing is one corner of the cube. The whole cube
passing is the proof.

## Fixture matrix -- the work list

Each "scenario" is a single combination of NTFS-volume parameters.
Together they span the matrix:

### Volume size axis

- 32 MiB -- below chkdsk shadow-copy threshold; tests minimum
  viability.
- 64 MiB -- chkdsk reports a shadow-copy warning but still completes.
- 256 MiB -- our default during the bug-fix loop; chkdsk /scan works.
- 1 GiB -- mid-volume MFT placement may differ.
- 4 GiB -- crosses the 32-bit-LBA boundary if anyone treats it that
  way; surfaces sign / overflow bugs.
- 16 GiB -- requires non-trivial MFT growth, large `$Bitmap`.
- (Add 64 GiB / 256 GiB only if the VM has the disk for it.)

### Cluster size axis

- 512 B (smallest -- one sector per cluster)
- 1 KiB
- 4 KiB (default for most volume sizes)
- 8 KiB
- 64 KiB (largest commonly used; tests big-cluster MFT layout)

### Label axis

- empty string
- "CITEST" (ASCII basic)
- 32 chars ASCII (max length per spec)
- "Disk \xC3\xA9clipse" (Latin-1 with diacritics)
- "日本語ラベル" (CJK)
- "Disk \xE2\x9A\xA1 emoji" (BMP emoji)

### Operation patterns axis (for the round-trip half)

- **Empty**: no files written -- baseline.
- **Tiny ASCII**: `tiny.txt` of "hello world" (resident `$DATA`).
- **Medium**: `medium.bin` of 4 KiB random (boundary; resident or not).
- **Large**: `big.bin` of 4 MiB (definitely non-resident; multiple
  cluster runs).
- **Many small**: 256 files of 16 bytes each (stresses `$Bitmap` and
  `$I30` index growth).
- **Deep nesting**: 8 levels of nested directories with one file at
  the leaf.
- **Long names**: files with 255-character names (max NTFS name).
- **Unicode names**: files with CJK / emoji names.
- **Sparse**: 1 GiB sparse file with 2 small populated runs.

Not every cell in the (size × cluster × label × ops) cube is a useful
test. The work list (`tests/matrix/work-list.json`) lists the cells
we actually want covered. Start with ~20 scenarios spanning the axes
and add coverage for any new bug we surface.

## Generating the fixtures

For Half 1 (Mac writes), the scenario parameters drive `mkfs_ntfs`'s
CLI directly:

```sh
./mkfs_ntfs --volume-size 256MiB --cluster-size 4096 \
  --label "CITEST" --serial deadbeefcafe1234 nfs.img
```

If a needed parameter isn't a CLI flag yet, the agent's first move is
to **add the flag** under `corroborated-debug` discipline (cite
publicly documented NTFS layout, add the smallest-possible parameter
plumbing, run `cargo test`, document the addition).

For "files written via our writer" scenarios, we need a Mac-side
write capability. Currently `mkfs_ntfs` only formats; it doesn't
write user files. **Treat that as a separate task** rather than
blocking on it: the agent who picks the first "Mac writer" scenario
either (a) finds we already have a write API (search for `write_file`
/ `create_file` in `src/`), (b) plumbs a minimal one against the
existing index and `$Bitmap` code with corroborated-debug discipline,
or (c) marks that scenario as "blocked on writer support" and picks a
different scenario.

For Half 2 (Windows writes), the writes are Windows-native PowerShell
ops on the mounted drive letter:

```pwsh
Set-Content -Path "${letter}:\tiny.txt" -Value "hello world" -NoNewline
[System.IO.File]::WriteAllBytes("${letter}:\medium.bin", (New-Object byte[] 4096))
```

## Parallel test execution

Two layers of parallelism:

### Layer 1 -- Multiple test scenarios per build (Windows-side)

Build `mkfs_ntfs.exe` ONCE per source state. Then dispatch N scenarios
in parallel via PowerShell `Start-Job`, each with:

- Its own `nfs-<scenario>.img`
- Its own `wrapper-<scenario>.vhdx`
- Its own `reference-<scenario>.vhdx` (for the byte-diff)
- Its own drive letter (D, E, F, G, H, ...)
- Its own diag dir under `diag/<scenario>/`

A 5-wide pool runs the matrix at ~5x throughput. Add more parallelism
only if VM CPU/RAM is the bottleneck; with disk-mount serialised by
the kernel the practical ceiling is ~5-8 concurrent VHDX mounts.

The aggregate verdict: any failing scenario fails the run. Each
scenario's diag dir comes back to the Mac.

### Layer 2 -- Multiple agent instances (Mac-side)

Multiple agent instances pick scenarios from the work list, each
fixing whatever bug their scenario surfaces. Coordination is
file-based, not git-based:

- Work list lives at `tests/matrix/work-list.json` -- a JSON object
  keyed by scenario name with status fields.
- Agents pick the first scenario whose status is `pending` and
  atomically transition it to `claimed-<session-name>` via a
  rename-then-fsync pattern.
- Done scenarios get status `passed-<session-name>` or
  `failed-<session-name>` with a link to the iteration entry.

To avoid races on the work list itself, the rename uses a temporary
file and `mv` (atomic on POSIX). Agents check after the rename that
their session won the claim (read-back); if not, they pick another
scenario.

## The agent's session

Each agent instance, on starting, MUST:

1. **Pick a unique session name.** Format: `agent-<random4>-<isodate>`.
   E.g. `agent-3f7c-2026-05-02`. The session name appears in every
   commit message, every claim file, every findings entry the agent
   writes.

2. **Read the three skills** (`dev-loop`, `corroborated-debug`,
   `documentation-protocol`) and the latest `docs/chkdsk-findings.md`.

3. **Claim a scenario** from `tests/matrix/work-list.json`.

4. **Run the scenario.**
   - Half 1: drive the Mac → Windows → Mac round-trip via
     `scripts/test-windows-local.sh` (parameterised by scenario name).
   - Half 2: same script, opposite direction.

5. **If the scenario surfaces a bug**, enter the corroborated-debug
   loop. Use the local-pipeline byte-diff. Make ONE minimal change
   per iteration. Append a findings-doc entry per iteration. Verify
   with `cargo test`.

6. **When the scenario passes** (or the agent runs out of useful
   work), update the work list with the final status and stop.

7. **Never push to `origin/main`.** Commit on the worktree branch.
   Main thread merges only after manual review.

## Done criteria

The matrix is "done" when **all of**:

- Every scenario in `tests/matrix/work-list.json` has status `passed-*`.
- All Linux tests still pass on the merged main.
- Local pipeline produces a clean chkdsk verdict (Stage 1 + Stage 2)
  on the default scenario after the merge.
- `docs/chkdsk-findings.md` ends with a "Conclusion" section
  summarising total iterations, root-cause clusters, and what's
  deliberately deferred.

After done, this document and the work list become read-only history
for the next class of bugs.

## Invariants every agent must enforce

- **Linux test contract**: `cargo test --release --lib mkfs --test
  mkfs_roundtrip --test mkfs_bin_smoke` must pass after every change.
  If a change makes a test fail, fix the change, not the test.
- **Lint contract**: `cargo fmt --check` and `cargo clippy
  --all-targets -- -D warnings` must pass. The pre-commit hook
  enforces this; an agent that bypasses the hook with `--no-verify`
  has violated the protocol.
- **GPL-tooling rule** (project memory, hard rule): no mention of
  `ntfs-3g`, `mkntfs`, `ntfsfix`, `ntfsinfo`, `Tuxera`, `e2fsprogs`,
  `mke2fs`, or any GPL'd reverse-engineered NTFS implementation
  anywhere -- not in source, not in comments, not in docs, not in CI,
  not in commit messages. Use generic phrasing only ("the canonical
  Linux NTFS reimplementation", "publicly documented NTFS layout").
  Citations come from Microsoft MS-FSCC, Windows Internals, or our
  own byte-diff observations -- never from Linux NTFS project docs.
- **No bundled changes**: one fix per commit, one bug per iteration.
  The skill explicitly forbids "I'll change A and B then run." If
  one fixes the symptom, you don't know which.
- **Worktree isolation**: each agent runs in its own git worktree,
  pushes to its own branch, never directly to `origin/main`.

## Parallel-test infrastructure files

The supporting code lives at:

- `tests/matrix/scenarios/` -- one TOML file per scenario describing
  parameters.
- `tests/matrix/work-list.json` -- shared queue.
- `tests/matrix/inspect/` -- small Mac-side CLI binary that uses
  `fs_ntfs`'s reader to enumerate a `.img` (for the Mac-verify legs
  of the round-trip).
- `scripts/run-windows-matrix.ps1` -- parallel test runner on the VM
  (builds once, dispatches N scenarios via Start-Job).
- `scripts/test-windows-matrix.sh` -- Mac-side orchestrator.
- `scripts/agent-bootstrap.sh` -- helper agents source to claim a
  scenario from the work list.

These files don't all exist yet. They are part of the deliverable for
the first agent that picks a scenario requiring them. That agent
treats "scaffold the matrix infrastructure" as their iteration's task,
under the same skills discipline.

## Concurrency rules (HOW to self-resolve clashes)

The single overriding rule is **never block waiting for a human**.
Every conflict has a deterministic resolution below; if you encounter
one not listed here, pick the resolution that minimises damage to
other agents' in-flight work and proceed.

### Work-list claim race

Two agents tried to claim the same scenario.

- After your atomic-rename claim, **read the lock file back**. If its
  `session` field is yours, you won. If it isn't, you lost.
- The losing agent picks the next pending scenario and tries again.
- Never delete or overwrite another session's claim file.

### Merge conflict on `docs/chkdsk-findings.md`

Both agents appended an iteration entry. Git auto-merges these because
they're disjoint sections, but a textual conflict can still arise if
two agents both touched the same line.

- Always run `git pull --rebase origin <integration-branch>` (or
  equivalent for the merging branch) before pushing your worktree
  branch.
- If a rebase produces a conflict in the findings doc: **always
  preserve both iteration entries**. Keep both `### iter<N>` and
  `### iter<N+1>` sections; renumber yours to be the next available
  number after the highest existing one. Do this without asking.
- If a rebase conflict arises in `src/mkfs.rs`: **abort the rebase**,
  re-fetch the latest integration branch, redo your change against
  the new base by re-reading the byte-diff that justified it (the
  evidence is still valid; the line numbers just moved). If after
  re-reading the diff your fix is no longer the minimal change (e.g.
  another agent already fixed an overlapping field), mark your
  scenario `superseded-by-<other-session>` and stop.

### Concurrent VHDX-mount on the VM

The Windows VM has a global drive-letter namespace and a global
`Mount-DiskImage` cache. Two agents mounting different VHDXs
simultaneously can collide on:

- Drive-letter assignment (Windows tries D, E, F... in order).
- Disk number (`Get-Disk -Number` is process-wide).
- The reference VHDX (only one `format.com` runs at a time).

Resolution:

- Each agent uses a unique VM workdir: `VM_WORKDIR=C:/Users/chris/dev/rust-fs-ntfs-<session-name>`.
- Each agent's runner script picks an unused drive letter dynamically
  via the `Set-Partition -NewDriveLetter` fallback (already in the
  current runner). If the dynamic letter pick fails, retry with a
  randomised choice from D-Z up to 3 times before marking the
  scenario `infra-flake-retry-later`.
- Format.com runs are short (<5 s); the runner serialises them
  internally with a short retry loop. If format.com fails because
  another agent's reference is mid-format, sleep 5 s and retry up to
  3 times.

### Worktree branch collision

Each agent works in `.claude/worktrees/agent-<session>/` on branch
`worktree-agent-<session>`. The session-name uniqueness rule
prevents collision; if you somehow encounter an existing branch with
your session name, append `-r2`, `-r3`, ... until unique. Do not
delete another session's worktree.

### Scope conflict (your fix overlaps another agent's in-flight fix)

You picked a scenario whose root cause turns out to be the same byte
field another agent is already fixing on a different scenario.

- If their fix has already landed on the integration branch and your
  diff against the new base shows the bug is gone: mark your
  scenario `passed-implicitly-by-<other-session>` and verify with the
  local pipeline.
- If their fix is in flight and you can see their commit on their
  branch but not on integration: WAIT (with a sleep + recheck loop)
  for up to 10 minutes. If still not landed, mark your scenario
  `blocked-on-<other-session>` and pick another.

### Test runner gets stuck on the VM

A previous agent crashed mid-mount and left a VHDX attached. The
current agent's mount fails because Windows reports "drive in use" or
similar.

- Run a cleanup PowerShell command at the start of every test:
  ```pwsh
  Get-DiskImage -ImagePath "$pwd\*.vhdx" -ErrorAction SilentlyContinue |
      Where-Object Attached -eq $true |
      Dismount-DiskImage -ErrorAction SilentlyContinue
  ```
- The runner already does this. If you discover an orphaned mount
  outside your workdir (i.e. another agent's VHDX), do NOT touch it.
  Just wait 60 s and retry your own mount.

### Unresolvable infrastructure failure

The VM is unreachable, or a tool you depend on is missing.

- Mark your scenario `blocked-infra-<reason>-<session>` with a
  findings-doc entry. Pick another scenario. Do NOT attempt to
  reinstall toolchains -- that's a setup-script change outside the
  agent's scope.

## Anti-patterns specific to multi-agent runs

- **Two agents claiming the same scenario.** Mitigated by atomic
  claim renames; if it happens anyway, the second agent backs off and
  picks another.
- **Agent A's fix breaking agent B's scenario.** Mitigated by the
  worktree-per-agent isolation. A bug fix that breaks another
  scenario gets caught when the merge step re-runs the matrix on the
  combined source.
- **Findings-doc merge conflicts.** Each agent appends to the doc.
  When two agents both append, git merges cleanly because they're
  appending different sections. If they happen to update the same
  section: last-writer-wins is fine for an append-only log (the
  iteration entries don't depend on each other).
- **A scenario silently passing because the agent didn't actually run
  the verify legs.** Mitigated by the `passed-*` status in work-list
  requiring a link to the iteration's diag dir. Reviewers can spot a
  scenario marked passed with no evidence.
- **An agent that runs out of skill** (e.g. the bug needs deeper code
  changes than the scenario warrants) **silently giving up.**
  Mitigated by requiring the work-list status to be either `passed-*`
  or `blocked-<reason>-<session>`. Blocked is fine; silent abandonment
  is not.

## System prompt (use this verbatim when spawning a new agent instance)

When spawning an agent instance with this protocol, paste the text
in `docs/multi-agent-test-protocol-prompt.md` as the agent's initial
prompt. That file is the agent-facing version of this document --
shorter, action-focused, references back here for full context.

The agent prompt explicitly tells the agent:

- It is one of N concurrent instances; other agents are racing it.
- It must never block on human input.
- Conflicts auto-resolve per the rules in this document's
  "Concurrency rules" section.
- The human is asleep; if the agent finds itself stuck, it picks
  another scenario rather than waiting.
