# 09 — Reproducing the Results

> *None of this is a trust exercise. Every claim in these documents is a command
> you can run. This page is the runbook.*

The suite is tiered by setup cost. You can verify the foundation in seconds with
nothing but a Rust toolchain, and progressively reproduce more of the stack as
you add fixtures, a nightly toolchain, and (for the top layer) a Windows VM.

```
   Tier 0  ·  no setup          ·  cargo test --lib        ·  525 tests, seconds
   Tier 1  ·  no setup          ·  cargo test -p am-fs-core ·   88 tests, seconds
   Tier 2  ·  + disk fixtures   ·  cargo test --tests       ·  645 tests
   Tier 3  ·  + nightly + fuzz  ·  cargo +nightly fuzz run   ·  3 targets
   Tier 4  ·  + Windows VM      ·  scripts/matrix-baseline.sh ·  44 chkdsk scenarios
```

---

## Tier 0 — the always-green gate (no setup)

The 525 unit tests in `src/` need no fixtures, no network, no special platform.
This is the gate that is always green:

```bash
cargo test --lib
#  → test result: ok. 525 passed; 0 failed; 0 ignored
```

Count them yourself:

```bash
grep -rhc '#\[test\]' src/*.rs | paste -sd+ - | bc        # → 525
```

---

## Tier 1 — the block-device substrate (no setup)

```bash
cargo test -p am-fs-core
grep -rhc '#\[test\]' vendor/rust-fs-core/tests/*.rs | paste -sd+ - | bc   # → 88
```

---

## Tier 2 — the integration tests (needs disk fixtures)

The 645 integration tests in `tests/` split into two kinds:

- **Self-generating tests** (most write / structural / field-exhaustion / capi
  tests) format a fresh volume in-memory via the crate's own `format_filesystem()`
  and need no external fixture.
- **Fixture-driven read-path tests** open pre-built `.img` files under
  `test-disks/`. Those images are **intentionally not committed** (they are large
  binaries) and are listed in `.gitignore`.

Build the read-path fixtures, then run everything:

```bash
# Generates real NTFS images inside a qemu-hosted Alpine VM
# (the portable way to format/loop-mount NTFS on macOS or CI).
bash test-disks/build-ntfs-feature-images.sh

cargo test --tests
grep -rhc '#\[test\]' tests/*.rs | paste -sd+ - | bc      # → 645  (across 89 files)
ls tests/*.rs | wc -l                                      # → 89
```

> **If you skip the fixture step**, the fixture-dependent files fail fast with a
> message pointing you here — that is expected, not a hidden failure. The
> self-generating tests still pass. CI (`.github/workflows/ci.yml`) installs qemu,
> runs the generator, and then `cargo test`; it caches `test-disks/.vm-cache/` so
> repeat runs skip the ISO download.

---

## Tier 3 — fuzzing (needs nightly + cargo-fuzz)

```bash
rustup toolchain install nightly
cargo install cargo-fuzz

# Each target hammers one decoder; invariant = no panic / no OOB / no hang.
for t in decode_runs decode_eas iter_attributes; do
    cargo +nightly fuzz run "$t" -- -max_total_time=60
done

ls fuzz/fuzz_targets/*.rs | wc -l                          # → 3
```

The regression-guard benchmarks (not a pass/fail gate):

```bash
cargo bench --bench byte_decoders   # Criterion; HTML report under target/criterion/
```

---

## Tier 4 — the real-Windows `chkdsk` matrix (needs a Windows VM)

This is the authoritative layer. It requires a reachable Windows VM provisioned
per `scripts/setup-windows-vm.sh`, with connection details in a local-only
`.test-env` (never committed). See
[06 — The Windows `chkdsk` matrix](06-windows-chkdsk-matrix.md) for what it does.

```bash
# Inspect the scenario work-list and its current status:
python3 -c "import json;d=json.load(open('test-matrix.json'));print(len(d['scenarios']),'scenarios')"   # → 44

# Fast sanity gate — 5 representative scenarios (a few minutes):
bash scripts/matrix-baseline.sh --smoke

# Full matrix — all 44 scenarios, ~30 min (fanned out across the parallel VM pool):
bash scripts/matrix-baseline.sh

# Compare two runs (non-zero exit on regression):
bash scripts/matrix-diff.sh OLD/matrix-results.json test-diagnostics/matrix-results.json

# Verify the current tree is SEALED (its committed result matches a fresh build):
bash scripts/matrix-verify.sh           # exits 0 iff sealed
bash scripts/matrix-verify.sh --build    # rebuild first if stale
```

The run writes `test-diagnostics/matrix-results.json` recording the Windows
build, `ntfs.sys` version, `chkdsk` version, per-scenario exit codes, and the
`binary_sha256` seal — so any verdict is fully attributable to an exact binary on
an exact Windows build.

---

## Reproduce every headline number on the index

```bash
echo "unit:        $(grep -rhc '#\[test\]' src/*.rs                  | paste -sd+ - | bc)"   # 525
echo "integration: $(grep -rhc '#\[test\]' tests/*.rs                | paste -sd+ - | bc)"   # 645
echo "substrate:   $(grep -rhc '#\[test\]' vendor/rust-fs-core/tests/*.rs | paste -sd+ - | bc)"  # 88
echo "test files:  $(ls tests/*.rs | wc -l)"                                                  # 89
echo "fuzz:        $(ls fuzz/fuzz_targets/*.rs | wc -l)"                                       # 3
echo "scenarios:   $(python3 -c "import json;print(len(json.load(open('test-matrix.json'))['scenarios']))")"  # 44
```

---

## Local hygiene gates (what a contributor's machine enforces)

```bash
bash scripts/install-hooks.sh   # installs the pre-commit hook (local-only config)
# thereafter, every commit must pass:
cargo fmt --check
cargo clippy -- -D warnings
```

These are not part of the test counts above, but they keep the suite honest:
no change lands without the fast gate green and the lints clean.

---

**Back to:** [the index →](README.md)
