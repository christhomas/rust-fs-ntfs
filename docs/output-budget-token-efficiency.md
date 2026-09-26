# Output budgets and agent token efficiency

This note describes the output-budget migration in
[`rust-fs-ntfs` PR #326](https://github.com/christhomas/rust-fs-ntfs/pull/326).
The shared wrapper is owned by `rust-fs-core`; its related follow-up work is
tracked in
[`rust-fs-core` issue #153](https://github.com/antimatter-studios/rust-fs-core/issues/153).

## Current design

`scripts/tier.sh` assigns each test tier measured line and byte limits, asks
Cargo where the resolved `am-fs-core` package lives, and makes a transient copy
of core's canonical `scripts/output-budget.sh`. The copy is removed when the
tier exits. This keeps the wrapper version tied to Cargo resolution without a
stale repository copy.

`cargo metadata --locked` also fetches and unpacks a missing registry source,
so a separate fetch step is unnecessary even with an empty Cargo cache. The
discovery probe clears `RUSTFLAGS` and `RUSTDOCFLAGS` only for metadata; this
keeps it on the default toolchain when an ASan tier supplies nightly-only
flags, while the wrapped test command still receives those flags unchanged.

On success, the command's output stays in `tmp/logs/<tier>.log`. The terminal
gets one verdict containing the tier, line and byte counts, and full log path:

```text
unit: ok (653 lines, 44377 bytes) — …/tmp/logs/unit.log
```

A successful command that exceeds its tier budget exits 65 and points to the
full log. A log containing `SKIP:` exits 66; ignored-test counts remain visible.
`OUTPUT_BUDGET_VERBOSE=1` or the supported verbose CLI flag streams the same output as
well as retaining it, but does not relax the budget.

## Logs and CI artifacts

Local logs remain under `tmp/logs/`. GitHub Actions uploads that directory with
`if: always()`, so logs are retained for both green and red jobs. Current
artifact names are:

- `test-logs-${{ matrix.os }}` for the main OS matrix;
- `test-logs-integration` for the fixture-backed integration job;
- `test-logs-asan` for the advisory AddressSanitizer job; and
- `test-logs-release` for the release suite.

The upload steps use `if-no-files-found: ignore`. Artifacts therefore contain
the logs that were actually created; a failure before log creation does not
manufacture an empty log.

## Failure capsules

For a failed wrapped command, the canonical wrapper emits a compact capsule:

- the tier label and exact underlying exit status;
- no log lines at all by default, and the last N when `--tail N` or
  `OUTPUT_BUDGET_FAIL_TAIL=N` asks for them;
- the total log line count; and
- the full local log path.

The workflow step or local `scripts/tier.sh <tier> -- <command>` invocation
provides command context, while the tier label identifies the corresponding
policy and log. The tail normally exposes the failing test or command
diagnostic without replaying the entire transcript. `tier.sh` returns the
wrapped command's status unchanged. Adapter-owned failures, such as Cargo being
unable to resolve a core package containing the wrapper, fail separately with
a concise diagnostic before a tier log can exist.

## Why this saves tokens

Successful build and test transcripts are repetitive and rarely affect the
next decision. Reducing them to one verdict prevents agents from repeatedly
ingesting compiler progress and passing-test output. On failure, the bounded
tail supplies the immediately useful evidence, while the complete log remains
available locally and as a CI artifact for deeper diagnosis. The design
therefore reduces routine context use without discarding debugging data or
changing command exit semantics.

## Tradeoffs

- The tail can omit an earlier root cause; diagnosis may require opening the
  full artifact.
- Fixed line and byte budgets require deliberate updates as legitimate output
  grows, and measurements can vary across toolchains and platforms.
- A tier label is less specific than a fully serialized command. Command
  details remain in the workflow step or local invocation.
- Quiet output provides less sense of progress during long runs. Verbose mode
  is available when live progress is worth the extra output.
- CI artifacts are available only after upload runs and remain subject to the
  repository's artifact retention settings.

## Follow-on ideas

- **Structured JSON summaries:** record tier, command, status, counts, timing,
  log path, and failure-tail metadata for reliable machine consumption.
- **Deduplication:** collapse repeated compiler or test diagnostics in the
  displayed capsule while leaving the full log unchanged.
- **Step summaries:** publish concise GitHub Actions summaries linking each
  tier verdict to its artifact.
- **Event-driven polling:** let agents wait for job or artifact state changes
  instead of repeatedly fetching unchanged CI output.

These are possible extensions, not behavior provided by PR #326.
