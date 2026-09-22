# Working in rust-fs-ntfs (agent guide)

Pure-Rust NTFS driver exposing a C ABI, validated against Windows `chkdsk` as
the oracle. This file is the fast path for an agent picking up work here, so
you don't re-derive the workflow each time. It points at the existing docs
rather than duplicating them:

- **README** → `## Test contract`, `## Building`, `## What works` / `## What doesn't work`.
- **docs/multi-agent-test-protocol.md** → running several agents at once.
- **`.claude/skills/windows-test-skill/`** → the 42-scenario matrix discipline,
  the tiered gates, and the `matrix-results.json` seal.

<!-- BEGIN SHARED BLOCK: claiming-work v1 -->
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
<!-- END SHARED BLOCK: claiming-work v1 -->

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

## Output is budgeted — do not work around it

Every tier runs through `scripts/tier.sh`, which runs the suite **quietly**:
the whole run goes to `tmp/logs/<tier>.log`, a pass prints one verdict line
naming that log, and a failure prints its tail. CI uploads every log as the
`test-logs-*` artifact, so the detail is always retrievable.

The budget is a cap on the log, not just on what is shown, and each tier's
number was measured:

- a run that **passes but prints more than its budget** exits **65**;
- a run whose tests printed `SKIP:` exits **66** — a skipped test is not a
  passing test;
- otherwise the suite's own status is passed through.

`FWTH_VERBOSE=1` streams the run as well as logging it. It does **not** lift
the budget.

If a tier legitimately grows, raise its row in `scripts/tier.sh` **with the
measurement that justifies it** — the table says where every number came from.
Do not silence output to fit, and do not route around `tier.sh`.

The wrapper itself belongs to `rust-fs-core` and is resolved by
`scripts/resolve-output-budget.sh`, which validates it by SHA-256 and API
version. Never copy it into this repository.

## Changes to on-disk behaviour need the matrix

Anything that changes what is written to a volume must be validated against
Windows `chkdsk` before it lands — `validate rust-ntfs format (Windows chkdsk)`
is a required check on `main`. Read the `windows-test-skill` skill before
starting that work; the matrix is not something to drive from first principles.

Docs-only and CI-only changes skip the matrix, and that is why it reports
`skipping` rather than failing on them.

## Conventions

- Branches: `<type>/<short-name>`, matching the commit type — `fix/`, `feat/`,
  `ci/`, `docs/`, `chore/`, `test/`.
- Commit subjects are a declarative sentence, not an imperative phrase:
  "the run-end bound is checked", not "check the run-end bound".
- `main` takes **squash merges only**, and branch protection is declared in
  `.github-guard`, read from the server copy — not the working tree.
