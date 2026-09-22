# CLAUDE.md

The agent guide for this repository is **[AGENTS.md](AGENTS.md)** — read it
first. It is tool-agnostic, so every agent working here follows the same
instructions rather than one set per assistant.

It covers, in particular:

- **[Claiming work](AGENTS.md#claiming-work)** — claim an issue with the
  `claimed` label before you start, so two agents do not do the same work.
- **[Running tests](AGENTS.md#running-tests)** — the `chore` tasks CI runs.
- **[Output is budgeted](AGENTS.md#output-is-budgeted--do-not-work-around-it)**
  — why a passing tier prints one line, and what exit 65 and 66 mean.
- **[Changes to on-disk behaviour need the matrix](AGENTS.md#changes-to-on-disk-behaviour-need-the-matrix)**
  — the Windows `chkdsk` gate.
