# Working in rust-fs-ntfs (agent guide)

Pure-Rust NTFS driver exposing a C ABI, validated against Windows `chkdsk` as
the oracle. This file is the fast path for an agent picking up work here, so
the workflow does not have to be re-derived each time. It points at the
existing docs rather than duplicating them:

- **README** → `## Test contract`, `## Building`, `## What works` / `## What doesn't work`.
- **docs/multi-agent-test-protocol.md** → running several agents at once.
- **`.claude/skills/windows-test-skill/`** → the 42-scenario matrix discipline,
  the tiered gates, and the `matrix-results.json` seal.

The section between the BEGIN/END markers below is **shared, byte-identical,
with every repository in this family**. Do not edit it here: change the
canonical copy and propagate it, or `chore lint` will fail. Everything after
the END marker is specific to this repository.

<!-- BEGIN SHARED BLOCK: agent-core v1 sha256:60fad6dd98e9da3e9256d38728b02ac189dca0d04fc98c13e2c67de3f3103319 -->
## Claiming work

Several agents work these repositories at the same time. Before you start on
an issue, claim it, so nobody else spends a session on what you are already
doing. The lock is a **GitHub label**, because labels are shared state that
every agent can read and change without posting comments into the thread.

**Before starting.** Check, claim, then read back:

```sh
gh issue view <N> --json labels                      # holds `claimed`? pick another
gh issue edit <N> --add-label claimed --add-label claim/<session>
gh issue view <N> --json labels                      # read back and confirm
```

`<session>` is your session name — `agent-<random4>-<isodate>`, e.g.
`agent-3f7c-2026-09-22`. Create the `claim/<session>` label if it does not
exist.

**Resolving a race.** Adding a label is not compare-and-swap: two agents can
both add `claimed` and both believe they won. That is what the read-back is
for. If it shows more than one `claim/*` label, the **lexically lowest**
session keeps the issue; every other agent removes its own `claim/*` label and
picks different work. Each racer computes the same answer independently, so no
further coordination is needed.

**When you finish or stop.** Remove both labels — on merge, or the moment you
abandon the work:

```sh
gh issue edit <N> --remove-label claimed --remove-label claim/<session>
```

Delete your `claim/<session>` label from the repository at the end of your
session so they do not accumulate.

**Reclaiming a stale claim.** An agent that dies holding a claim would block an
issue forever. If `claimed` was applied more than 12 hours ago and the holder's
branch has no commits since, any agent may take it: remove the stale `claim/*`,
add your own, and say so in the issue.

**This is a convention, not a fence.** Nothing enforces it. An agent that
ignores it duplicates work; it cannot corrupt anything. Honour it anyway.

## Skills to use

- **`dev-loop`** — the required loop for any non-trivial change: baseline the
  full suite → change → re-run (no baseline test may regress) → enhance tests →
  vet. Always run it.
- **`commit`** / **`pr`** — for grouping commits and opening pull requests.

Each repository names any further skills of its own below.

## A bug fix starts with a red

**Prove it is broken first** — a failing check or test — *then* fix it, *then*
prove that same check is green, *then* confirm the full baseline still passes.
Never write the fix before you have a red. A fix with no failing test to its
name is a claim, not a result.

## Nothing skips

A test that cannot run **fails**, naming the task that would provide what it
needed. Never add an early return for a missing fixture, tool or VM: a skipped
test reads exactly like a passing one, and a suite that quietly declines to run
is indistinguishable from a suite that passes.

Where a tier reports skips or ignored tests, that is a gate, not a note.

## Validate against something that is not us

A driver's own readers share its interpretation of the format, so they cannot
catch a misreading: the mistake is baked into the fixture *and* the parser, and
they agree with each other while disagreeing with every real filesystem. Unit
tests over self-built fixtures prove self-consistency, not correctness.

Every structure that is parsed or written gets a cross-validation test against
an **independent oracle** — the platform's own tools, a real kernel, or a third
implementation — before it is considered done. Each repository names its
oracles below.

## Output is budgeted

Test tiers run through `scripts/tier.sh`, which runs the suite **quietly**: the
whole run goes to `tmp/logs/<tier>.log`, a pass prints one verdict line naming
that log, and a failure prints its tail. CI keeps the logs as an artifact, so
the detail is always retrievable.

The budget caps the log, not merely what is shown, and every number in the
table was measured. A run that passes but prints more than its budget **fails**.

The reader who pays most for a noisy suite is an agent that re-reads its whole
transcript on every step, and so pays for one loud run many times over. If a
tier legitimately grows, raise its row **with the measurement that justifies
it**. Do not silence output to fit, and do not route around `tier.sh`.

## Commits and branches

- Branches are `<type>/<name>`, matching the commit type: `fix/`, `feat/`,
  `ci/`, `docs/`, `chore/`, `test/`.
- A commit is a subject plus flat one-sentence bullets. Subjects are
  declarative, not imperative: "the run-end bound is checked", not "check the
  run-end bound".
- **No AI attribution and no co-author trailers**, in commits or in pull
  request descriptions.
- `main` takes **squash merges only**.

## Project rules

- **No GPL/LGPL/AGPL dependencies.** Permissive only (MIT/BSD/Apache).
  Shelling out to a copyleft CLI as a *test oracle* is fine — linking or
  copying it is not.
- **Each of these is a standalone project.** Never mention a consuming
  application in the README, the source, or CLI help.
<!-- END SHARED BLOCK: agent-core v1 -->

## Skills specific to this repository

- **`windows-test-skill`** — the 42-scenario Windows matrix: tiered gates
  (`cargo test` → smoke matrix → full matrix), the `matrix-results.json` seal,
  and the staging-branch integration workflow. Read it before any work that
  changes what is written to a volume.

## Running tests

The tasks are the interface; CI runs exactly these:

```sh
chore siblings          # ../rust-fs-core and ../fs-windows-test-harness at their pinned tags
chore siblings:check    # report drift, change nothing
chore lint              # fmt + clippy, exactly as CI runs them
chore test:unit         # unit tests (src/), release
chore test:unit:debug   # unit tests, debug, overflow checks armed
chore test:mkfs         # the two fixture-free integration files
chore test:suite        # every unit and integration test, release
chore test              # every tier CI runs, quietly
chore test:scripts      # the shell tests (tests/scripts/*.sh)
chore test:ignored      # the #[ignore]d tests: known defects, run so a fix cannot go unnoticed
chore matrix:check      # lint test-matrix.json against the harness config (no VM)
chore matrix -- smoke   # matrix scenarios on the Windows VM
```

## The oracle here is Windows chkdsk

Our own readers cannot prove the bytes we wrote are right, so a real Windows
`chkdsk` grades them across the 42-scenario matrix. Anything that changes what
is written to a volume must pass it before it lands: `validate rust-ntfs format
(Windows chkdsk)` is a required check on `main`.

Docs-only and CI-only changes report `skipping` for that context rather than
failing, which is why they can merge without a VM run.

## How the budget is wired here

`scripts/tier.sh` owns the table of tiers and their measured budgets. The
wrapper doing the work belongs to `rust-fs-core`: `tier.sh` asks cargo where
the resolved `am-fs-core` package is, checks that its
`scripts/output-budget.sh` answers `--version` with
`rust-fs-core-output-budget 1`, and copies it into `tmp/` for the run.
**Never copy that wrapper into this repository.**

It used to be a separate `scripts/resolve-output-budget.sh` pinning the
script's SHA-256. The digest is gone on purpose: pinned in every consumer, it
meant a comment added in core broke each of them until the digest was chased,
which is the lockstep one canonical copy exists to remove.

Three exit statuses are worth knowing apart:

- **65** — the run passed but printed more than its budget.
- **66** — a test printed `SKIP:` and was counted as passing.
- anything else — the suite's own status, passed straight through.

`OUTPUT_BUDGET_VERBOSE=1` streams the run as well as logging it, and does not
lift the budget. It was `FWTH_VERBOSE` while the wrapper was the Windows
harness's; the old name is not read, and setting it does nothing.

A **failing** tier is quiet too, from core v0.2.13: the verdict, the exit
status and the log's path. `--tail N`, or `OUTPUT_BUDGET_FAIL_TAIL=N`, prints
the tail for whoever is watching.

## What gates a merge

Five required contexts on `main`: the full test suite, `test-macos-latest`,
`test-ubuntu-latest`, `test-ubuntu-24.04-arm`, and the Windows chkdsk
validation. Protection is declared in `.github-guard` and read **from the
server copy of the default branch**, never from the working tree — which is
what stops a branch checkout from unprotecting `main`.

`scripts/agents-core-check.sh` verifies the shared block above is intact.
