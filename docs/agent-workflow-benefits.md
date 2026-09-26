# Agent workflow improvements and benefits

This document summarises the improvements made to the issue-processing and
test-validation workflow, with token usage and reliable coordination as the
top priorities.

## The central improvement

Routine work is now quiet and decision-oriented:

- successful commands emit one compact verdict;
- full output is retained in a local log and, in CI, as an artifact;
- failed commands emit a bounded diagnostic capsule rather than replaying the
  entire transcript; and
- the full log is opened only when the capsule is not enough to diagnose the
  failure.

This preserves the evidence needed for debugging while avoiding the repeated
ingestion of compiler progress, passing-test names, and unchanged CI output.

## Benefits

### Lower token usage

Passing test suites no longer consume context with thousands of repetitive
lines. CI polling can inspect job state first and fetch output only for a
failed job. The agent therefore spends tokens on decisions and fixes instead
of reproducing already-successful work.

### Better failure signal

Failure capsules include the tier, exit status, line count, log location, and
the final diagnostic lines. This usually identifies the failing test quickly,
while the complete transcript remains available for root-cause analysis.

### Reliable evidence without noisy conversations

Logs are uploaded even when a job fails, and successful runs report their
artifact and verdict paths. A PR or issue can therefore cite reproducible
evidence without copying a large transcript into comments.

### Enforced output budgets

Each test tier has an explicit line and byte budget. A command that succeeds
but becomes unexpectedly noisy fails the budget check, making output growth
visible instead of allowing token costs to drift silently.

### Safer multi-agent coordination

Issues are claimed through shared labels before implementation. A claim has a
unique session label, is read back before work starts, and is released only
after terminal CI evidence is available. This makes ownership visible without
adding comments to issue threads and prevents two agents from solving the same
issue concurrently.

### Serial, auditable issue processing

Processing one claimed issue at a time keeps the worktree and evidence easy to
reason about. Each completed issue has a clear chain:

```text
claim → implement → focused check → CI → issue evidence → release claim
```

The issue tracker becomes both the queue and the coordination record. A PR is
not treated as complete merely because it exists; the claim remains until the
required validation is terminal and green.

### Appropriate validation cost

The change classifier determines whether a patch needs the Windows/chkdsk
harness. Filesystem-format and on-disk behavior changes receive the expensive
cross-platform validation; documentation-only or read-path-only changes can
use the lighter matrix when Windows coverage is not relevant. This saves
compute and output without weakening validation where it matters.

### Clear separation of responsibilities

Implementation work is delegated to the coding worker, while administration
tracks claims, CI state, PR linkage, and issue evidence. This keeps each agent's
context focused and makes handoffs less error-prone.

## Current operating pattern

For each issue:

1. Inspect labels and existing work.
2. Claim the issue with a unique session label.
3. Have the implementation worker make the smallest useful change.
4. Run the focused regression and required local gates.
5. Open or update the PR and wait for the relevant CI matrix.
6. Read logs only if a job fails; otherwise record the compact result.
7. Post links, commit, and validation evidence to the issue.
8. Remove the claim labels only after terminal evidence.

This gives the next agent a concise, machine-readable state without requiring
it to reconstruct the history from long comments or raw logs.

## Further mechanical improvements

These extensions can reduce usage further while preserving the evidence model:

- emit a small JSON result per tier containing status, timing, counts, log
  path, and failure-tail metadata;
- deduplicate repeated compiler and test diagnostics in displayed capsules;
- publish one GitHub Actions summary table with links to each tier artifact;
- poll job state and timestamps instead of repeatedly fetching unchanged logs;
- cache a compact issue/claim snapshot for queue selection;
- use focused artifact download commands that retrieve only the failed tier;
- keep a short per-issue evidence template so comments remain consistent; and
- add elapsed-time and output-size trends to detect regressions in CI cost.

The guiding rule is simple: keep complete evidence durable, but keep agent
messages proportional to the decision they enable.

## Related documentation

The implementation details for tier budgets, retained logs, artifacts, and
failure capsules are in
[`output-budget-token-efficiency.md`](output-budget-token-efficiency.md).
