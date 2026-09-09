//! The debug run that lets the PR gate see an overflow guards itself.
//!
//! `overflow-checks` is on in debug and off in release, so a defect
//! whose only symptom is an arithmetic overflow panic cannot be
//! observed by a release-only test run. Measured on this crate rather
//! than inferred from `[profile.dev]` carrying no key: a temporary
//! `black_box(250u8) + black_box(10u8)` panicked under `cargo test
//! --locked` and passed under `cargo test --locked --release`.
//!
//! So the debug run in `ci.yml` -- the workflow that actually gates a
//! merge -- is what makes every arithmetic test in this crate mean
//! anything, and this file is what keeps it there. Nothing else in the
//! repository would notice if that run were deleted, or if `--release`
//! were added to it.
//!
//! THE GUARD IS NOT PROTECTION AGAINST MALICE. It is protection
//! against a tidy-up. Where a repository runs the suite twice, the
//! debug job looks like a duplicate of the release one beside it, and
//! that is exactly why someone removes it. Where it runs once, the
//! plausible edit is the opposite -- adding `--release` for
//! consistency with a sibling or to cut CI time -- which turns the
//! checks off with nothing failing. Both edits are silent; this file
//! is what makes them loud.
//!
//! # Two halves, neither redundant
//!
//! | half | asks | cannot answer |
//! |---|---|---|
//! | the scans here | is the step still in `ci.yml`, asked to check, and not disabled from the manifest | whether the build it produces actually traps |
//! | `overflow_checks` in `src/lib.rs` | does this build trap a real `u64::MAX + 1` | whether it was supposed to; it cannot notice its own absence |
//!
//! Delete the step and the runtime probe never runs at all. Keep the
//! step but drop the variable and the probe runs, finds nothing to
//! check, and passes doing nothing. Keep both and put
//! `overflow-checks = false` under `[profile.test]` and the step is
//! present, running, green and blind. Each needs its own guard.
//!
//! # Why this is an integration test and not a module under `src/`
//!
//! Cargo discovers `tests/*.rs` on its own, so there is no declaration
//! anywhere that can be deleted to switch this off, and `Cargo.toml`
//! sets no `autotests = false`. A guard living as a file under `src/`
//! behind a `#[cfg(test)] mod` line has no such protection: lose the
//! one line and the file stays, compiles into nothing, and asserts
//! nothing, with no lint to say so. That happened once already on a
//! sibling repository's version of this fix -- a `git reset --hard`
//! took the `mod` line, the suite went green, and seven assertions
//! silently ceased to exist.
//!
//! The runtime probe in `src/lib.rs` is the deliberate exception, and
//! is inline in `lib.rs` for the same reason: it must be part of the
//! library target the debug step builds, and inline there is no
//! declaration to lose.

use saphyr::{LoadableYamlNode, Yaml};
use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn ci_yml() -> PathBuf {
    manifest_dir()
        .join(".github")
        .join("workflows")
        .join("ci.yml")
}

/// Read a file the guards depend on, or fail.
///
/// It panics rather than returning `None` on purpose. An
/// `if !path.exists() { return }` anywhere in this module would
/// reproduce the exact class of blindness the module exists to prevent:
/// an assertion that is present, runs, and cannot report the thing it
/// was written for. A missing workflow is a finding, not a skip.
fn read_or_panic(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e}. This guard must fail rather than skip: a \
             version of it that returned early here would be the same \
             blindness it exists to prevent.",
            path.display()
        )
    })
}

/// Whether a token is a leading `NAME=value` shell assignment.
///
/// This is how the handshake is passed —
/// `EXPECT_OVERFLOW_CHECKS=1 cargo test ...` — so the command word is
/// not always the first token. A name is the shell's: letters, digits
/// and underscores, not starting with a digit. `--test=qemu` is not
/// one of these, which is what keeps [`cargo_test_arguments`] from
/// mistaking an option for an assignment.
fn is_shell_assignment(token: &str) -> bool {
    match token.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && !name.starts_with(|c: char| c.is_ascii_digit())
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

/// The arguments of a `cargo test` INVOCATION, or `None` if the line
/// is not one.
///
/// # WHY THE COMMAND WORD AND NOT A SUBSTRING
///
/// The check here was `command.contains("cargo test")`, which counts a
/// line that merely prints the command:
///
/// ```text
/// - run: |
///     echo "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib"
///     cargo test --release --locked --lib
/// ```
///
/// That satisfied BOTH workflow assertions — the debug run and the
/// handshake — with no debug run behind either, and this repository's
/// `ci.yml` already prints quoted commands in its sibling-checkout
/// step, so the shape is live rather than contrived. It is one of the
/// four bypasses rust-fs-ntfs#248 names, and the only one the first
/// fix left open.
///
/// So the line has to BE the invocation: leading `NAME=value`
/// assignments, then `cargo` (however it is spelled on disk), then an
/// optional `+toolchain`, then the `test` subcommand. Anything in
/// front of `cargo` — `echo`, `printf`, `:` — means the text is being
/// quoted rather than run.
///
/// The returned slice is the arguments after `test`, which is what
/// [`names_one_integration_target`] reads.
fn cargo_test_arguments<'a>(tokens: &'a [&'a str]) -> Option<&'a [&'a str]> {
    let mut rest = tokens;
    while rest.first().is_some_and(|token| is_shell_assignment(token)) {
        rest = &rest[1..];
    }
    let (program, after_program) = rest.split_first()?;
    if program.rsplit('/').next() != Some("cargo") {
        return None;
    }
    rest = after_program;
    while rest.first().is_some_and(|token| token.starts_with('+')) {
        rest = &rest[1..];
    }
    let (subcommand, arguments) = rest.split_first()?;
    if *subcommand != "test" {
        return None;
    }
    Some(arguments)
}

/// Whether the run selects a single integration target.
///
/// `--test <name>` builds one integration target and no library unit
/// tests, so it does not cover the crate's arithmetic. Several
/// repositories here run a cross-validation suite that way, in its own
/// job, beside the real one.
///
/// # WHY THIS READS THE OPTION INSTEAD OF MATCHING `"--test "`
///
/// The trailing space was there to protect `--tests`, which DOES build
/// the library unit tests and must still count:
///
/// ```text
/// "--test " in "--all-targets"  -> false
/// "--test " in "--tests"        -> false   (so it counts)
/// "--test"  in "--tests"        -> true    (so it would not)
/// ```
///
/// But a space is not the only separator an option takes, so
/// `--test=qemu_validation` matched neither and counted as a full
/// debug run. WIDENING THE SUBSTRING IS THE WRONG FIX — dropping the
/// space is exactly what the space is for, and that is how it became
/// load-bearing in the first place. Comparing whole arguments answers
/// both spellings at once and cannot be undone by the next separator.
///
/// Scanning stops at a bare `--`: what follows is handed to the test
/// binary, and `cargo test --lib -- --test=x` still builds the
/// library.
fn names_one_integration_target(arguments: &[&str]) -> bool {
    arguments
        .iter()
        .take_while(|argument| **argument != "--")
        .any(|argument| *argument == "--test" || argument.starts_with("--test="))
}

/// Cargo's short options that CONSUME A VALUE. The rest of a merged
/// cluster after one of these is that value, not more flags.
const SHORT_OPTIONS_TAKING_A_VALUE: &[char] = &['p', 'j', 'F', 'Z'];

/// Whether the run asks for a profile whose `overflow-checks` this file
/// cannot vouch for.
///
/// # `-r` IS `--release`, AND THE SUBSTRING COULD NOT SEE IT
///
/// This was `command.contains("--release") || command.contains("--profile")`.
/// Cargo's short form of `--release` is `-r`, so
///
/// ```text
/// cargo test --locked -r --all-targets
/// ```
///
/// contains neither string. A workflow whose only test run was that
/// line satisfied every assertion in this file with `overflow-checks`
/// OFF -- the one state the whole file exists to make impossible. The
/// runtime probe in `src/lib.rs` does not catch it either: it asserts
/// only when `EXPECT_OVERFLOW_CHECKS` is set, and this shape sets it,
/// on a release run.
///
/// WIDENING THE SUBSTRING IS THE WRONG FIX, and it is worth saying out
/// loud because this is the third time in this one file. `contains("-r")`
/// matches `--release`, `--target-dir`, `--no-run` and any word carrying
/// those two characters, so the guard would begin refusing correct
/// workflows. Whole arguments are compared instead.
///
/// # THE SHORT CLUSTER IS PARSED, NOT SEARCHED FOR AN `r`
///
/// clap merges short flags, so `-qr` is `--quiet --release` and must be
/// caught. But four of cargo's shorts take a value which may be glued
/// to them: `-p rust-fs-ntfs` can be written `-prust-fs-ntfs`, whose
/// second character is `r` and which selects a package, not a profile.
/// So a cluster is read left to right and stops at the first
/// value-taking short, which is how clap reads it.
///
/// Scanning stops at a bare `--`: what follows is the test binary's
/// argument, not cargo's.
///
/// `--profile` disqualifies whatever it names, including `dev`. That is
/// the pre-existing rule and it is deliberately over-strict: this file
/// reads `Cargo.toml` for `dev` and `test` only, so a run under a third
/// profile is one whose checks it has not established.
fn selects_a_release_profile(arguments: &[&str]) -> bool {
    for argument in arguments.iter().take_while(|argument| **argument != "--") {
        if *argument == "--release"
            || *argument == "--profile"
            || argument.starts_with("--profile=")
        {
            return true;
        }
        let Some(cluster) = argument.strip_prefix('-') else {
            continue;
        };
        // A long option, or a bare `-`; neither is a short cluster.
        if cluster.starts_with('-') || cluster.is_empty() {
            continue;
        }
        for flag in cluster.chars() {
            if flag == 'r' {
                return true;
            }
            if SHORT_OPTIONS_TAKING_A_VALUE.contains(&flag) {
                break;
            }
        }
    }
    false
}

/// Every `cargo test` invocation in a shell script that would be
/// compiled with overflow checks on.
///
/// The argument is the SHELL text of one step's `run:`, not YAML.
/// [`parse_workflow`] has already turned the workflow into a structure,
/// so a YAML comment can no longer reach this function at all -- that
/// half of the old scan is now the parser's job, by construction.
///
/// The `#` handling below is still load bearing, because what does
/// reach here is shell, and shell has comments of its own inside a
/// `run: |` block.
///
/// Four things disqualify a command, and each one is a way the guard
/// could otherwise be satisfied by something that does not actually
/// build in debug:
///
/// - it is a shell comment;
/// - it is an inline trailing comment on an otherwise-`--release` line;
/// - it passes `--release` in either spelling, or names a profile
///   explicitly ([`selects_a_release_profile`]);
/// - it sets a `CARGO_PROFILE_*` variable, which can turn overflow
///   checks off for the dev or test profile from outside the manifest.
///
/// A `cargo build` is not a `cargo test` and is not considered, nor is
/// any step that invokes no cargo at all.
///
/// # PARSING THE WORKFLOW WAS NECESSARY AND NOT SUFFICIENT
///
/// The YAML is read properly now, and the two remaining defeats were
/// substring matches applied to the parsed value — the predicate, not
/// the parsing. They are read as arguments instead: see
/// [`cargo_test_arguments`] for the command,
/// [`names_one_integration_target`] for the target selection, and
/// [`selects_a_release_profile`] for the profile, which was the third
/// and was found the same way as the first two.
fn runs_with_overflow_checks(script: &str) -> Vec<String> {
    script
        .lines()
        .filter_map(|raw| {
            let line = raw.trim_start();
            if line.starts_with('#') {
                return None;
            }
            let command = line.split(" #").next().unwrap_or(line).trim();
            let tokens: Vec<&str> = command.split_whitespace().collect();
            let arguments = cargo_test_arguments(&tokens)?;
            if selects_a_release_profile(arguments) {
                return None;
            }
            if command.contains("CARGO_PROFILE_") {
                return None;
            }
            if names_one_integration_target(arguments) {
                return None;
            }
            Some(command.to_string())
        })
        .collect()
}

/// WHAT ELSE DECIDES WHETHER A STEP GATES.
///
/// The first version of this guard matched the text of a `- run:` line
/// and never looked at anything else in the step. That is enough to
/// find the command and useless for deciding whether the command's
/// result is read. Measured against this repository's own workflow:
/// adding `if: false` to the step, or `continue-on-error: true`, left
/// every one of the guard's 31 tests green while the gate went blind.
/// A step that runs and whose result nothing reads is this
/// constellation's own named defect, reproduced inside the guard
/// written to prevent it.
///
/// So the list is enumerated first, rather than discovered one defeat
/// at a time. A `run:` step gates a pull request only if ALL of these
/// hold:
///
/// 1. the step carries no `if:` -- a false condition skips it;
/// 2. the step carries no `continue-on-error:` -- its failure is
///    discarded;
/// 3. its JOB carries no `if:` -- same reasoning, one level up;
/// 4. its JOB carries no `continue-on-error:`;
/// 5. the workflow's `on:` still includes `pull_request` -- a scan
///    scoped to `ci.yml` assumes `ci.yml` is what runs on a pull
///    request, and that is a fact about the file, not a given.
///
/// OVER-STRICT IS THE SAFE DIRECTION HERE, so 1 and 2 reject on the
/// key's PRESENCE rather than trying to evaluate it. `if: false`,
/// `if: ${{ false }}`, and an `if:` on an expression that happens to
/// evaluate false are distinct spellings, and this crate has already
/// been caught by four spellings of one manifest key -- enumerating
/// them is the losing game. A step that genuinely needs a condition
/// can be split out; a guard that tries to interpret conditions is a
/// guard with a new defeat every time GitHub adds syntax.
///
/// # Why this is parsed and no longer scanned
///
/// The version this replaces hand-rolled the YAML: `.lines()`, an
/// indent count, `split_once(':')` for the key, and `after != "|"` for
/// a block scalar. It was defeated three more times after the five
/// spellings above, and each defeat was the same shape -- ordinary
/// YAML the scanner had not been taught:
///
/// ```text
///   "if": false               quoted key -- matched no NON_GATING_KEYS
///                             entry, so the step counted as gating
///                             while Actions skipped it. SILENT.
///   "continue-on-error": true same.
///   # pull_request:           a substring match over the `on:` block's
///                             raw text, comments included, so
///                             commenting the trigger out left the
///                             guard green. SILENT.
///   run: |-  / run: >         only a bare `|` opened a block, so every
///                             other legal style was read as the
///                             command itself and the block's contents
///                             never parsed. LOUD -- it failed a
///                             correct workflow.
/// ```
///
/// Quoted keys, block scalar styles, comments and nested mappings are
/// not edge cases; they are the grammar. A parser handles all of them
/// by construction, and does not need to be taught the next one. The
/// sibling `rust-fs-xfs` copy patched each hole individually and its
/// own comments record the cost: the identical quote-normalisation was
/// added to its TOML key scan, and then had to be added again, a few
/// dozen lines away, to its YAML key scan. The same lesson twice in one
/// file is the argument against learning it a third time.
///
/// `saphyr` is a dev-dependency, so nothing here reaches a consumer of
/// the crate.
#[derive(Debug)]
struct Step {
    keys: Vec<String>,
    run: String,
    /// The step's `env:` mapping, as `KEY=VALUE`.
    ///
    /// The handshake is an environment variable, and an inline
    /// `VAR=1 cargo test` prefix is bash syntax. A matrix that includes
    /// `windows-latest` runs the same step under PowerShell, where that
    /// prefix is a syntax error -- so on a cross-platform crate the
    /// handshake HAS to be declared here rather than in the command,
    /// and a guard that only reads the command would refuse the only
    /// spelling that works.
    env: Vec<String>,
}

#[derive(Debug)]
struct Job {
    keys: Vec<String>,
    steps: Vec<Step>,
}

#[derive(Debug)]
struct Workflow {
    triggers: Vec<String>,
    jobs: Vec<Job>,
}

/// The value of `name` in a YAML mapping, or `None`.
///
/// By name rather than by constructing a key, because `saphyr`'s `Yaml`
/// borrows the source text and building one to hand to `get` is more
/// ceremony than the lookup is worth here.
fn field<'a, 'b>(node: &'a Yaml<'b>, name: &str) -> Option<&'a Yaml<'b>> {
    node.as_mapping()?
        .iter()
        .find(|(key, _)| key.as_str() == Some(name))
        .map(|(_, value)| value)
}

/// An env value as text, whether it was written `1`, `"1"` or `true`.
///
/// `EXPECT_OVERFLOW_CHECKS: 1` and `EXPECT_OVERFLOW_CHECKS: "1"` are
/// the same variable to Actions, and a guard that accepted only the
/// quoted spelling would be back to matching spellings.
fn scalar_text(node: &Yaml) -> Option<String> {
    if let Some(text) = node.as_str() {
        return Some(text.to_string());
    }
    if let Some(i) = node.as_integer() {
        return Some(i.to_string());
    }
    node.as_bool().map(|b| b.to_string())
}

/// The keys of a YAML mapping, as plain strings.
///
/// The parser has already resolved the quoting, so `"if"`, `'if'` and
/// `if` all arrive here as `if`. That is the whole of the quoted-key
/// fix: there is no un-quoting step to forget.
fn keys_of(node: &Yaml) -> Vec<String> {
    node.as_mapping()
        .map(|mapping| {
            mapping
                .iter()
                .filter_map(|(key, _)| key.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Structure a workflow far enough to answer the five questions above.
///
/// Panics on a workflow it cannot parse, deliberately. A guard that
/// returned an empty `Workflow` for a file it did not understand would
/// report "no debug run gates this" -- which is a failure, so that
/// direction is safe -- but a guard that returned early with a PASS
/// would be the blindness this module exists to prevent. Failing on the
/// parse error names the real problem instead of a consequence of it.
fn parse_workflow(text: &str) -> Workflow {
    let documents = Yaml::load_from_str(text).unwrap_or_else(|e| {
        panic!(
            "workflow is not valid YAML: {e}. This guard reads the workflow \
             rather than scanning its text, so a file it cannot parse is a \
             failure and never a pass."
        )
    });
    let Some(document) = documents.first() else {
        return Workflow {
            triggers: Vec::new(),
            jobs: Vec::new(),
        };
    };

    // `on:` takes three legal shapes: a mapping of trigger names, a
    // sequence of them, or a single scalar. All three are names.
    //
    // Note that `on` survives as the string key `on` and is not folded
    // into the boolean `true` -- saphyr implements the YAML 1.2 core
    // schema, where only `true`/`false` are booleans. The YAML 1.1
    // reading that would break every GitHub workflow ever written does
    // not apply.
    let triggers = match field(document, "on") {
        Some(on) if on.as_mapping().is_some() => keys_of(on),
        Some(on) if on.as_sequence().is_some() => on
            .as_sequence()
            .into_iter()
            .flatten()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect(),
        Some(on) => on.as_str().map(str::to_string).into_iter().collect(),
        None => Vec::new(),
    };

    let mut jobs = Vec::new();
    if let Some(mapping) = field(document, "jobs").and_then(Yaml::as_mapping) {
        for (_, body) in mapping.iter() {
            let steps = field(body, "steps")
                .and_then(Yaml::as_sequence)
                .into_iter()
                .flatten()
                .map(|step| Step {
                    keys: keys_of(step),
                    env: field(step, "env")
                        .and_then(Yaml::as_mapping)
                        .map(|m| {
                            m.iter()
                                .filter_map(|(k, v)| {
                                    Some(format!("{}={}", k.as_str()?, scalar_text(v)?))
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    // A `run:` block of any style -- `|`, `|-`, `|+`,
                    // `>`, `>-`, `|2` -- arrives as one string with the
                    // block folded per its own rules, so a command
                    // inside a shell loop is seen whole rather than as
                    // fragments, and no style is mistaken for the
                    // command itself.
                    run: field(step, "run")
                        .and_then(Yaml::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect();
            jobs.push(Job {
                keys: keys_of(body),
                steps,
            });
        }
    }

    Workflow { triggers, jobs }
}

/// Does this workflow still run on a pull request at all?
///
/// A whole-name comparison against the parsed trigger keys. The version
/// this replaces asked `wf.triggers.contains("pull_request")` of the
/// `on:` block's raw text -- comments and blank lines included -- so
/// the word appearing anywhere in it satisfied the guard. Commenting
/// the real key out, or deleting it and leaving a comment naming it,
/// left `ci.yml` no longer running on pull requests at all with the
/// guard still green.
///
/// `pull_request_target` DELIBERATELY DOES NOT COUNT, and the omission
/// is the point rather than an oversight. It runs against the base
/// repository with a write token and the repository's secrets, and it
/// checks out the base ref by default -- so a workflow triggered only
/// that way may never build the contributor's code at all, and
/// accepting it as proof the merge is gated is permissive in the worst
/// direction. `rust-fs-xfs#146` and `rust-fs-ext4#149` record it as a
/// live gap in the hand-rolled guard this file replaces, where the
/// clause was written by hand and then copied between repositories.
///
/// A parser has no opinion about `pull_request_target` unless someone
/// writes one. So it is not written. If this repository ever needs it
/// accepted, that is a decision with its own justification, and it
/// comes with a check that the checkout selects the pull request head.
fn runs_on_pull_request(wf: &Workflow) -> bool {
    wf.triggers.iter().any(|t| t == "pull_request")
}

/// Keys whose presence on a step or job means its result does not gate.
const NON_GATING_KEYS: [&str; 2] = ["if", "continue-on-error"];

/// Walk a workflow's steps and collect what `select` finds in each
/// `run:`.
///
/// `gating` restricts the walk to steps whose result the pull-request
/// gate actually reads: the workflow must still trigger on a pull
/// request, and neither the job nor the step may carry a key from
/// [`NON_GATING_KEYS`].
///
/// One walk rather than two. The headline assertion used the line-based
/// scan while only the handshake assertion was step-aware, so under
/// `if: false` the headline PASSED and its failure message would have
/// claimed the pull-request gate could see an overflow when the step it
/// names does not run. Every defeat spelling still turned the suite red
/// through the other assertion, so this was a precision defect rather
/// than a hole -- but it left the "runs without --release" property
/// verified line-based, and defeatable if the handshake assertion were
/// ever weakened. Both halves share this walk now and cannot drift
/// apart again. Found on the sibling `rust-fs-btrfs` copy of this guard
/// and corrected here rather than left to diverge.
fn collect_steps(workflow: &str, gating: bool) -> Vec<Step> {
    let wf = parse_workflow(workflow);
    if gating && !runs_on_pull_request(&wf) {
        return Vec::new();
    }
    let carries_a_non_gating_key =
        |keys: &[String]| keys.iter().any(|k| NON_GATING_KEYS.contains(&k.as_str()));

    let mut out = Vec::new();
    for job in wf.jobs {
        if gating && carries_a_non_gating_key(&job.keys) {
            continue;
        }
        for step in job.steps {
            if gating && carries_a_non_gating_key(&step.keys) {
                continue;
            }
            out.push(step);
        }
    }
    out
}

fn scan_steps(workflow: &str, gating: bool, select: fn(&str) -> Vec<String>) -> Vec<String> {
    collect_steps(workflow, gating)
        .iter()
        .flat_map(|step| select(&step.run))
        .collect()
}

/// Does this step ask the build to prove it traps an overflow?
///
/// Either spelling counts, and both are the same instruction to
/// Actions: the variable inline in the command, or declared in the
/// step's `env:` mapping. The mapping is not a concession -- it is the
/// ONLY spelling that works on a matrix including `windows-latest`,
/// where an inline `VAR=1 cargo test` prefix is a PowerShell syntax
/// error. A guard that read only the command would refuse the correct
/// workflow on every cross-platform crate in this constellation.
fn step_declares_the_handshake(step: &Step) -> bool {
    step.run.contains("EXPECT_OVERFLOW_CHECKS=1")
        || step
            .env
            .iter()
            .any(|entry| entry == "EXPECT_OVERFLOW_CHECKS=1")
}

/// The run commands of steps that run in debug AND actually gate a
/// pull request -- without requiring the handshake.
fn gating_runs_with_overflow_checks(workflow: &str) -> Vec<String> {
    scan_steps(workflow, true, runs_with_overflow_checks)
}

/// The run commands of steps that both run in debug with the handshake
/// AND actually gate a pull request.
///
/// Step-aware rather than text-aware, because the handshake may be
/// declared in the step's `env:` mapping rather than inline in the
/// command -- see [`step_declares_the_handshake`].
fn gating_runs_that_prove_the_build_traps(workflow: &str) -> Vec<String> {
    collect_steps(workflow, true)
        .into_iter()
        .filter(step_declares_the_handshake)
        .flat_map(|step| runs_with_overflow_checks(&step.run))
        .collect()
}

/// The debug runs that ask the build to prove it traps an overflow.
///
/// A subset of [`runs_with_overflow_checks`]: those which also set the
/// `EXPECT_OVERFLOW_CHECKS` handshake, so that
/// `overflow_checks::the_build_the_gate_asked_to_check_does_check`
/// performs an overflow and fails if the build let it through.
///
/// A run carrying the handshake but also `--release` is not counted,
/// because [`runs_with_overflow_checks`] has already excluded it. Such
/// a step is a misconfiguration and it fails loudly rather than
/// quietly: the checks are legitimately off in release, so the
/// assertion the handshake arms would fire there every time.
fn debug_runs_that_prove_the_build_traps(script: &str) -> Vec<String> {
    runs_with_overflow_checks(script)
        .into_iter()
        .filter(|command| command.contains("EXPECT_OVERFLOW_CHECKS=1"))
        .collect()
}

/// The guard. Reads the workflow this repository's pull requests are
/// gated by and refuses if nothing in it compiles the overflow checks.
///
/// `ci.yml` specifically, not every workflow -- see
/// [`a_checking_debug_run_that_is_not_in_ci_yml_does_not_satisfy_this_guard`],
/// which is the one repository-specific decision in this file.
#[test]
fn the_pr_gate_still_tests_in_a_profile_that_can_see_an_overflow() {
    let path = ci_yml();
    let workflow = read_or_panic(&path);

    let debug_runs = gating_runs_with_overflow_checks(&workflow);
    assert!(
        !debug_runs.is_empty(),
        "no `cargo test` in {} runs without `--release`, so a defect whose \
         only symptom is an arithmetic overflow panic can merge without the \
         PR gate ever seeing it. release.yml already runs a debug suite, and \
         that does not help: it triggers on a version tag, after the change \
         has merged. If the debug step in ci.yml looked redundant beside the \
         release ones, it is not -- see the comment above it.",
        path.display()
    );
}

/// The other half of the workflow scan: the step exists, but does it
/// ask the build anything?
///
/// # Why a handshake rather than more spellings
///
/// The manifest scan below reads `Cargo.toml` and asks whether a known
/// spelling of "overflow checks are off" is present. Several spellings
/// of the key were needed before it was right, and then routes turned
/// up that are not in that file at all: a
/// `CARGO_PROFILE_TEST_OVERFLOW_CHECKS` variable set at step or job
/// level in the workflow, and a `.cargo/config.toml`, which nothing
/// here reads. All of them leave the debug step present, running, green
/// and blind.
///
/// They are all the same shape: a scanner enumerating the ways a thing
/// can be disabled, in the places it happens to look. Another pass buys
/// the next one. So the question is put to the build instead -- perform
/// an overflow, see whether you are stopped -- and this test's job
/// shrinks to making sure the gate still asks it.
#[test]
fn the_debug_run_asks_the_build_to_prove_it_traps_overflows() {
    let path = ci_yml();
    let workflow = read_or_panic(&path);

    let proving = gating_runs_that_prove_the_build_traps(&workflow);
    assert!(
        !proving.is_empty(),
        "no `cargo test` in {} runs without `--release` while setting \
         EXPECT_OVERFLOW_CHECKS=1, so nothing checks whether the profile the \
         gate builds actually traps an arithmetic overflow. Reading \
         Cargo.toml is not enough: the checks can also be turned off by a \
         CARGO_PROFILE_TEST_OVERFLOW_CHECKS variable at step or job level, \
         or by a .cargo/config.toml, neither of which is in any file this \
         test reads. The handshake is what arms the one check that cannot be \
         fooled by where the setting lives.",
        path.display()
    );
}

/// THE DISTINCTION THIS REPOSITORY NEEDS THAT A PORTED COPY WOULD MISS.
///
/// A workflow carrying a checking debug run under a name other than
/// `ci.yml` -- `release.yml`, in this repository's own case -- must not
/// satisfy the guards above. Simulated here with `release.yml`'s actual
/// step shape: a plain `cargo test --locked --all-targets` with no
/// `EXPECT_OVERFLOW_CHECKS`, because that workflow was never asked to
/// carry the handshake and is not asked to.
///
/// The scenario worth pinning is the near miss: even a hypothetical
/// debug run in `release.yml` that DID set the handshake would not make
/// `ci.yml`'s own absence of one acceptable, because `release.yml`
/// triggers too late to gate a merge. Both shapes are asserted below.
///
/// This is why the scan is scoped to one file rather than globbed over
/// `.github/workflows/`. A workflow that triggers on a version tag runs
/// after the change has already merged, so a debug run there does not
/// gate anything; a scan across every workflow would count it and
/// report the gate as sound when no pull request is covered. The
/// guard's correctness therefore comes from WHICH FILE it opens, not
/// from the parser refusing these shapes, and that is a fact worth
/// pinning rather than leaving as a comment someone could stop
/// believing.
#[test]
fn a_checking_debug_run_that_is_not_in_ci_yml_does_not_satisfy_this_guard() {
    let release_yml_as_it_is = "\
jobs:
  test:
    steps:
      - run: cargo test --locked --all-targets
      - run: cargo test --locked --all-targets -- --ignored
";
    assert_eq!(
        scan_steps(release_yml_as_it_is, false, runs_with_overflow_checks),
        vec![
            "cargo test --locked --all-targets".to_string(),
            "cargo test --locked --all-targets -- --ignored".to_string(),
        ],
        "release.yml's real steps ARE debug runs -- the parser counts them, \
         and the only reason they do not satisfy the guard is that the guard \
         never opens that file"
    );
    assert!(
        scan_steps(
            release_yml_as_it_is,
            false,
            debug_runs_that_prove_the_build_traps
        )
        .is_empty(),
        "release.yml carries no handshake, and is not asked to"
    );

    let release_yml_with_a_handshake = "\
jobs:
  test:
    steps:
      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --all-targets
";
    assert_eq!(
        scan_steps(
            release_yml_with_a_handshake,
            false,
            debug_runs_that_prove_the_build_traps
        ),
        vec!["EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --all-targets".to_string()],
        "the parser itself would count this step too -- so widening the scan \
         to every workflow would silently stop catching this repository's \
         actual defect"
    );

    // And the real guards must be reading ci.yml, not one of these.
    let scanned = read_or_panic(&ci_yml());
    assert!(
        !gating_runs_that_prove_the_build_traps(&scanned).is_empty(),
        "the guards above must be satisfied by ci.yml's own content, not by \
         any of the strings in this test"
    );
}

/// The full dotted paths that switch overflow checks off for the
/// profile `cargo test` builds.
///
/// # This compares a whole path, because a key is not a word
///
/// The first version of this scan tracked the `[section]` and compared
/// the key to the literal `"overflow-checks"`. That reads correctly and
/// is defeated by ordinary TOML, because the same setting has several
/// spellings and cargo honours all of them without a warning. Measured
/// on a sibling repository with a runtime `u64::MAX + 1` unit test as
/// the probe -- `cargo test --locked --lib` EXIT=101 means the checks
/// are on, EXIT=0 means they are off, and `cargo metadata --no-deps`
/// was EXIT=0 for every one:
///
/// ```text
///   (nothing)                                          EXIT=101  on
///   [profile.test]  overflow-checks = false            EXIT=0    off
///   [profile.test]  "overflow-checks" = false          EXIT=0    off
///   [profile.test]  'overflow-checks' = false          EXIT=0    off
///   [profile]       test.overflow-checks = false       EXIT=0    off
/// ```
///
/// A bare key, a basic string, a literal string, and a dotted key that
/// puts the profile name on the key side where a section-matching scan
/// never looks. Four of those five defeated the first version, and each
/// leaves the debug step in `ci.yml` present, running, green and blind
/// -- the exact state the guard exists to refuse.
///
/// So the section and the key are joined into one path and normalised
/// per segment, and the comparison is against the whole thing. That
/// covers the spellings above, a quoted *section* (`["profile"."test"]`),
/// and a fully top-level dotted key with no section at all.
///
/// Only `profile.dev` and `profile.test` count. `cargo test` builds the
/// `test` profile, which inherits from `dev`, so either can disable the
/// checks in one line. `profile.release` is deliberately absent: the
/// checks are off there by default, that is what ships, and the release
/// steps exist to test what ships.
fn profiles_disabling_overflow_checks(manifest: &str) -> Vec<String> {
    /// Split a dotted TOML path and strip each segment's quoting, so
    /// that `"profile" . 'test'` and `profile.test` are one path.
    fn normalise(path: &str) -> String {
        path.split('.')
            .map(|segment| {
                segment
                    .trim()
                    .trim_matches(|c| c == '"' || c == '\'')
                    .trim()
            })
            .collect::<Vec<_>>()
            .join(".")
    }

    const DISABLED: [&str; 2] = [
        "profile.dev.overflow-checks",
        "profile.test.overflow-checks",
    ];

    let mut section = String::new();
    let mut found = Vec::new();
    for raw in manifest.lines() {
        let line = raw.split('#').next().unwrap_or(raw).trim();
        if line.starts_with('[') {
            section = normalise(line.trim_matches(|c| c == '[' || c == ']'));
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if value.trim() != "false" {
            continue;
        }
        let key = normalise(key);
        let path = if section.is_empty() {
            key
        } else {
            format!("{section}.{key}")
        };
        if DISABLED.contains(&path.as_str()) {
            found.push(path);
        }
    }
    found
}

/// The half of the property the workflow scans cannot see.
///
/// A debug step in `ci.yml` only buys anything while the profile it
/// builds actually checks. One line -- `overflow-checks = false` under
/// `[profile.test]`, or under this repository's existing
/// `[profile.dev]`, a plausible way to make a slow suite faster --
/// would leave that step present, running, green, and no longer able to
/// observe an overflow, with every workflow assertion above still
/// passing. A guard for half a condition is the defect it was written
/// to prevent.
///
/// The runtime probe in `src/lib.rs` would also catch this. This scan
/// is kept as defence in depth: it fails earlier in the gate and names
/// the offending manifest key, which is a better diagnostic than "the
/// build did not trap".
#[test]
fn the_profile_that_cargo_test_builds_still_checks_for_overflow() {
    let path = manifest_dir().join("Cargo.toml");
    let manifest = read_or_panic(&path);

    let disabled = profiles_disabling_overflow_checks(&manifest);
    assert!(
        disabled.is_empty(),
        "{} sets `overflow-checks = false` under {disabled:?}. `cargo test` \
         builds the `test` profile, which inherits from `dev`, so this \
         switches off the check that the debug step in ci.yml exists to run \
         -- leaving that step present, green, and blind. Put it back, or the \
         debug step is costing a compile and buying nothing.",
        path.display()
    );
}

/// The shell scanner is the part of this that can rot, so it is checked
/// against each shape it has to tell apart.
///
/// Its argument is the shell text of one step's `run:`, not YAML. What
/// used to be tested here as YAML -- a debug command quoted in a `#`
/// line of the workflow -- moved to `gating`, because the parser now
/// answers it by construction and this function never sees it.
mod shell_scan {
    use super::runs_with_overflow_checks;

    /// The trap this repository actually contains, in the form that
    /// still reaches this function. `ci.yml` documents the debug step
    /// by quoting the command, and a `run: |` block can carry the same
    /// habit in shell comments, where the text survives the command's
    /// deletion.
    #[test]
    fn a_debug_run_quoted_in_a_shell_comment_does_not_count() {
        let block = "\
set -euo pipefail
# Measured on this branch:
#     cargo test --locked --release --lib   ->  EXIT=0
#     EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib   ->  EXIT=101
cargo test --locked --release
";
        assert_eq!(
            runs_with_overflow_checks(block),
            Vec::<String>::new(),
            "a debug command quoted inside a comment is documentation, not a run"
        );
    }

    #[test]
    fn a_real_debug_run_counts() {
        let block = "\
cargo test --locked --release
EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib
";
        assert_eq!(
            runs_with_overflow_checks(block),
            vec!["EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib".to_string()],
        );
    }

    /// A command that is `--release` but which carries a trailing
    /// comment mentioning the debug run.
    #[test]
    fn a_trailing_comment_does_not_promote_a_release_run() {
        let inline = "cargo test --locked --release  # not cargo test --lib\n";
        assert_eq!(
            runs_with_overflow_checks(inline),
            Vec::<String>::new(),
            "the command is --release; the comment after it is not a second run"
        );
    }

    /// The inline-comment strip, which nothing else here pins. A real
    /// debug run whose trailing comment happens to contain `--release`
    /// must still be counted. Without the strip that word disqualifies
    /// the command, and the guard then fails insisting there is no
    /// debug run while one is sitting in front of it.
    #[test]
    fn a_trailing_comment_naming_release_does_not_disqualify_a_debug_run() {
        let line = "cargo test --locked --lib  # deliberately not --release\n";
        assert_eq!(
            runs_with_overflow_checks(line),
            vec!["cargo test --locked --lib".to_string()],
            "the command is a debug run; --release appears only in its comment"
        );
    }

    /// The ways a run can carry no `--release` and still be built
    /// without the checks.
    #[test]
    fn a_profile_named_another_way_does_not_count() {
        let lines = [
            "cargo test --locked --profile release-with-debug --lib",
            "CARGO_PROFILE_TEST_OVERFLOW_CHECKS=false cargo test --locked --lib",
            "CARGO_PROFILE_DEV_OVERFLOW_CHECKS=false cargo test --locked --lib",
        ];
        for line in lines {
            assert_eq!(
                runs_with_overflow_checks(line),
                Vec::<String>::new(),
                "{line} does not compile the overflow checks"
            );
        }
        assert_eq!(
            lines.len(),
            3,
            "the loop above must have examined every shape"
        );
    }

    /// CARGO'S SHORT `-r` IS `--release`.
    ///
    /// The substring this replaces saw neither `--release` nor
    /// `--profile` in any of these, so each one was counted as the
    /// debug run the whole overflow gate rests on -- with the checks
    /// off. The merged cluster is the one worth reading twice: clap
    /// accepts `-qr` as `--quiet --release`.
    #[test]
    fn the_short_release_flag_does_not_count() {
        let lines = [
            "cargo test --locked -r --all-targets",
            "cargo test --locked --all-targets -r",
            "cargo test --locked -qr --all-targets",
            "cargo test --locked -rq --all-targets",
            "cargo test --locked -vr --lib",
            "cargo test --locked -j4 -r --all-targets",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked -r --lib",
        ];
        for line in lines {
            assert_eq!(
                runs_with_overflow_checks(line),
                Vec::<String>::new(),
                "{line} builds the release profile, where overflow-checks are off"
            );
        }
        assert_eq!(
            lines.len(),
            7,
            "the loop above must have examined every shape"
        );
    }

    /// THE ACCEPTANCE HALF, and the reason the fix is a parse rather
    /// than a wider substring.
    ///
    /// Every line here contains the characters `-r` somewhere and none
    /// of them selects the release profile. `-p rust-fs-ntfs` may be
    /// written glued, and its second character is an `r`; four of
    /// cargo's shorts take a value that way. A guard that refused these
    /// would refuse correct workflows, which is the failure mode the
    /// substring version of this rule was one edit away from.
    #[test]
    fn a_short_option_whose_value_begins_with_r_still_counts() {
        let lines = [
            "cargo test --locked -p rust-fs-ntfs --all-targets",
            "cargo test --locked -prust-fs-ntfs --all-targets",
            "cargo test --locked -Freparse --all-targets",
            "cargo test --locked -j4 --all-targets",
            "cargo test --locked --features release-checks --all-targets",
            "cargo test --locked --features r --all-targets",
            "cargo test --locked --features=r --all-targets",
            "cargo test --locked --target-dir release-dir --all-targets",
            // Past the separator the argument is the test binary's.
            "cargo test --locked --all-targets -- -r",
        ];
        for line in lines {
            assert_eq!(
                runs_with_overflow_checks(line).len(),
                1,
                "{line} builds the dev profile and must still count"
            );
        }
        assert_eq!(
            lines.len(),
            9,
            "the loop above must have examined every shape"
        );
    }

    /// `cargo build` is not `cargo test`. A workflow that builds a
    /// binary and then exercises it with external tooling runs no test
    /// suite, and a scanner that counted `cargo build --release` would
    /// be looking at the wrong steps entirely.
    #[test]
    fn a_cargo_build_step_is_not_a_test_run() {
        let validate_job = "\
cargo build --locked --release
./target/release/some-tool --check /tmp/image
";
        assert_eq!(
            runs_with_overflow_checks(validate_job),
            Vec::<String>::new(),
            "building a binary is not running a test suite"
        );
    }

    /// A single integration target is not the crate's arithmetic.
    /// Several repositories here run a cross-validation suite as
    /// `--test <name>` in its own job, and counting it would let the
    /// real debug run be deleted with the guard still green.
    #[test]
    fn a_single_integration_target_does_not_count() {
        for line in [
            "cargo test --locked --features qemu-validation --test qemu_validation",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --test some_oracle",
        ] {
            assert_eq!(
                runs_with_overflow_checks(line),
                Vec::<String>::new(),
                "{line} builds one integration target and no library unit tests"
            );
        }
    }

    /// The near miss the whole-argument comparison protects: `--tests`
    /// DOES build the library unit tests and must still count. The
    /// substring this replaced needed a trailing space for exactly
    /// this, and that space is what let `--test=<name>` through.
    #[test]
    fn a_tests_flag_run_counts_which_is_what_the_trailing_space_protected() {
        assert_eq!(
            runs_with_overflow_checks("cargo test --locked --tests"),
            vec!["cargo test --locked --tests".to_string()],
        );
        assert_eq!(
            runs_with_overflow_checks("cargo test --locked --all-targets").len(),
            1,
            "`--all-targets` builds the library too"
        );
    }

    /// HOLE ONE: A PRINTED COMMAND IS NOT A RUN.
    ///
    /// `contains("cargo test")` counted the `echo`, so this block
    /// satisfied both workflow assertions with no debug run in it at
    /// all. `ci.yml` prints quoted commands in its sibling-checkout
    /// step, so this is a shape the file already has.
    #[test]
    fn an_echoed_command_is_not_a_run() {
        let block = "\
echo \"EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\"
cargo test --release --locked --lib
";
        assert_eq!(
            runs_with_overflow_checks(block),
            Vec::<String>::new(),
            "the only debug command here is inside an echo; the run is --release"
        );
        assert_eq!(
            super::debug_runs_that_prove_the_build_traps(block),
            Vec::<String>::new(),
            "and the handshake it prints does not arm anything either"
        );
    }

    /// The same, in every spelling that puts something in front of the
    /// command word.
    #[test]
    fn a_command_word_that_is_not_cargo_is_not_a_run() {
        for line in [
            "echo cargo test --locked --lib",
            "printf '%s\\n' 'cargo test --locked --lib'",
            ": cargo test --locked --lib",
            "echo \"EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\"",
        ] {
            assert_eq!(
                runs_with_overflow_checks(line),
                Vec::<String>::new(),
                "{line} prints the command rather than running it"
            );
        }
    }

    /// ACCEPTANCE FOR HOLE ONE. The command word is not always the
    /// first token, and every legal spelling must survive the change:
    /// the handshake is passed as a leading assignment, and a
    /// toolchain may be selected.
    #[test]
    fn the_ways_a_real_invocation_is_spelled_all_still_count() {
        for line in [
            "cargo test --locked --lib",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib",
            "EXPECT_OVERFLOW_CHECKS=1 RUST_BACKTRACE=1 cargo test --locked --lib",
            "cargo +nightly test --locked --lib",
            "/usr/local/bin/cargo test --locked --lib",
        ] {
            assert_eq!(
                runs_with_overflow_checks(line),
                vec![line.to_string()],
                "{line} is a real debug run and must still count"
            );
        }
    }

    /// HOLE TWO: `--test=<name>` IS THE SAME OPTION AS `--test <name>`.
    ///
    /// The substring `"--test "` saw only the space-separated form, so
    /// a cross-validation job written with an `=` counted as a full
    /// debug run and the real one could be deleted with the guard
    /// green.
    #[test]
    fn an_equals_separated_integration_target_does_not_count() {
        for line in [
            "cargo test --locked --features qemu-validation --test=qemu_validation",
            "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --test=some_oracle",
        ] {
            assert_eq!(
                runs_with_overflow_checks(line),
                Vec::<String>::new(),
                "{line} builds one integration target and no library unit tests"
            );
        }
    }

    /// ACCEPTANCE FOR HOLE TWO. An argument after a bare `--` goes to
    /// the test binary, not to cargo, so it selects nothing: the
    /// library is still built and the run still counts.
    #[test]
    fn an_argument_past_the_separator_does_not_select_a_target() {
        assert_eq!(
            runs_with_overflow_checks("cargo test --locked --lib -- --test=x"),
            vec!["cargo test --locked --lib -- --test=x".to_string()],
            "past `--` the option belongs to the harness; the library was built"
        );
    }
}

/// The handshake half of the shell scanner.
mod handshake {
    use super::debug_runs_that_prove_the_build_traps;

    #[test]
    fn a_debug_run_carrying_the_handshake_counts() {
        let script = "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n";
        assert_eq!(
            debug_runs_that_prove_the_build_traps(script),
            vec!["EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib".to_string()],
        );
    }

    /// A debug run that exists and asks the build nothing. Buys a
    /// compile and no information.
    #[test]
    fn a_debug_run_without_the_handshake_does_not_count() {
        let script = "cargo test --locked --lib\n";
        assert_eq!(
            debug_runs_that_prove_the_build_traps(script),
            Vec::<String>::new(),
            "the step is there but nothing checks the build it produced"
        );
    }

    /// A handshake on a release run proves nothing and must not satisfy
    /// this: the checks are off in release on purpose.
    #[test]
    fn the_handshake_on_a_release_run_does_not_count() {
        let script = "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --release\n";
        assert_eq!(
            debug_runs_that_prove_the_build_traps(script),
            Vec::<String>::new(),
        );
    }

    /// And quoted inside a shell comment, which is where a `run: |`
    /// block would explain it.
    #[test]
    fn the_handshake_quoted_in_a_comment_does_not_count() {
        let script = "#     EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n";
        assert_eq!(
            debug_runs_that_prove_the_build_traps(script),
            Vec::<String>::new(),
        );
    }
}

/// The manifest scanner, held to the shapes it has to tell apart. These
/// do not depend on this repository's own `Cargo.toml`, so they keep
/// meaning something after it changes.
mod manifest_parser {
    use super::profiles_disabling_overflow_checks;

    #[test]
    fn the_test_profile_disabling_the_checks_is_caught() {
        let manifest = "\
[profile.release]
lto = true

[profile.test]
overflow-checks = false
";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            vec!["profile.test.overflow-checks".to_string()],
        );
    }

    /// This repository has an explicit `[profile.dev]`, so this is the
    /// likeliest place the setting would actually arrive.
    #[test]
    fn the_dev_profile_disabling_the_checks_is_caught() {
        let manifest = "[profile.dev]\nopt-level = 1\noverflow-checks   =   false\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            vec!["profile.dev.overflow-checks".to_string()],
        );
    }

    /// Release is expected to have them off. Flagging it would make the
    /// guard fail on every correct manifest, which is the fastest way
    /// to get a guard deleted.
    #[test]
    fn the_release_profile_disabling_the_checks_is_not_flagged() {
        let manifest = "[profile.release]\noverflow-checks = false\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            Vec::<String>::new(),
        );
    }

    /// A commented-out line is not a setting -- the same trap as the
    /// workflow parser's, in the other file this module reads.
    #[test]
    fn a_commented_out_setting_is_not_a_setting() {
        let manifest = "[profile.test]\n# overflow-checks = false\nopt-level = 1\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            Vec::<String>::new(),
        );
    }

    /// The comment strip, which nothing else here pins. The realistic
    /// way this setting arrives is with its excuse on the same line,
    /// and it must still be caught: unstripped, the value reads
    /// `false  # speeds the suite up`, which is not `false`, and the
    /// guard waves through the exact edit it exists to catch.
    #[test]
    fn a_disabling_line_with_a_trailing_comment_is_still_caught() {
        let manifest = "[profile.test]\noverflow-checks = false  # speeds the suite up\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            vec!["profile.test.overflow-checks".to_string()],
        );
    }

    /// A different setting being `false` is not this setting being
    /// `false`. Without this the scanner could be keying on the value
    /// alone -- flagging any `= false` under those two sections -- and
    /// every other test here would still pass.
    #[test]
    fn another_setting_being_false_is_not_this_one() {
        let manifest = "[profile.test]\ndebug-assertions = false\nopt-level = 1\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            Vec::<String>::new(),
        );
    }

    /// THE SPELLINGS THAT DEFEATED THE FIRST VERSION. Each of these was
    /// measured to genuinely switch the checks off, with no warning
    /// from cargo -- see the table on
    /// `profiles_disabling_overflow_checks`. A guard that reads one
    /// spelling of a setting is a guard against typing it one way.
    #[test]
    fn a_double_quoted_key_is_the_same_key() {
        let manifest = "[profile.test]\n\"overflow-checks\" = false\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            vec!["profile.test.overflow-checks".to_string()],
        );
    }

    #[test]
    fn a_literal_quoted_key_is_the_same_key() {
        let manifest = "[profile.dev]\n'overflow-checks' = false\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            vec!["profile.dev.overflow-checks".to_string()],
        );
    }

    /// The one a section-matching scan cannot see at all: the profile
    /// name is on the key side, so the section is only `profile`.
    #[test]
    fn a_dotted_key_putting_the_profile_on_the_key_side_is_caught() {
        let manifest = "[profile]\ntest.overflow-checks = false\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            vec!["profile.test.overflow-checks".to_string()],
        );
    }

    /// And with no section header at all, which is still valid TOML.
    #[test]
    fn a_top_level_dotted_key_is_caught() {
        let manifest = "profile.test.overflow-checks = false\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            vec!["profile.test.overflow-checks".to_string()],
        );
    }

    #[test]
    fn a_quoted_section_is_the_same_section() {
        let manifest = "[\"profile\".'test']\noverflow-checks = false\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            vec!["profile.test.overflow-checks".to_string()],
        );
    }

    /// Release stays exempt in the dotted spelling too, or normalising
    /// the path would have quietly widened what the guard refuses.
    #[test]
    fn the_release_profile_is_exempt_in_the_dotted_spelling_too() {
        let manifest = "[profile]\nrelease.overflow-checks = false\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            Vec::<String>::new(),
        );
    }

    /// `true` is the state we want and must not be reported as the
    /// state we do not. Without this the scanner could be keying on the
    /// word `overflow-checks` alone and nothing here would notice.
    #[test]
    fn enabling_the_checks_explicitly_is_not_flagged() {
        let manifest = "[profile.test]\noverflow-checks = true\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            Vec::<String>::new(),
        );
    }
}

/// WHAT ELSE DECIDES WHETHER THE STEP GATES -- one test per item on the
/// enumerated list, because each is a separate way for the gate to go
/// blind with the command still present and still matching.
///
/// The version of this guard these replace matched the `- run:` line in
/// isolation. Measured against this repository's own workflow, `if:
/// false` and `continue-on-error: true` each left all 31 of its tests
/// green while the gate stopped gating.
mod gating {
    use super::gating_runs_that_prove_the_build_traps;

    /// The shape that does gate, as a control. Every test below is this
    /// with one thing added, so a failure here would mean the fixture
    /// is wrong rather than the property.
    const GATING: &str = "\
on:
  pull_request:
    branches: [main]
jobs:
  test:
    steps:
      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib
";

    #[test]
    fn the_control_shape_gates() {
        assert_eq!(
            gating_runs_that_prove_the_build_traps(GATING).len(),
            1,
            "the control must be counted, or every test below passes for the wrong reason"
        );
    }

    #[test]
    fn a_step_carrying_if_does_not_gate() {
        for condition in [
            "if: false",
            "if: ${{ false }}",
            "if: github.event_name == 'push'",
            "if: ${{ env.SOMETHING == 'yes' }}",
        ] {
            let yaml = GATING.replace(
                "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n",
                &format!(
                    "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n        {condition}\n"
                ),
            );
            assert!(
                gating_runs_that_prove_the_build_traps(&yaml).is_empty(),
                "a step carrying `{condition}` may or may not run, so it cannot be what \
                 makes the gate able to see an overflow. Rejected on the key's presence \
                 rather than by evaluating it -- the spellings are open-ended."
            );
        }
    }

    #[test]
    fn a_step_carrying_continue_on_error_does_not_gate() {
        let yaml = GATING.replace(
            "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n",
            "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n        continue-on-error: true\n",
        );
        assert!(
            gating_runs_that_prove_the_build_traps(&yaml).is_empty(),
            "the step runs and its failure is discarded, which is the project's own named \
             defect: a step that runs and whose result nothing reads"
        );
    }

    #[test]
    fn a_job_carrying_if_does_not_gate() {
        let yaml = GATING.replace("  test:\n", "  test:\n    if: false\n");
        assert!(
            gating_runs_that_prove_the_build_traps(&yaml).is_empty(),
            "the same reasoning one level up: a job that may not run cannot gate"
        );
    }

    #[test]
    fn a_job_carrying_continue_on_error_does_not_gate() {
        let yaml = GATING.replace("  test:\n", "  test:\n    continue-on-error: true\n");
        assert!(
            gating_runs_that_prove_the_build_traps(&yaml).is_empty(),
            "a job whose failure is discarded cannot gate, however sound its steps"
        );
    }

    /// The assumption the `ci.yml`-only scan rests on, which is a fact
    /// about the file rather than a given.
    #[test]
    fn a_workflow_that_no_longer_runs_on_pull_request_does_not_gate() {
        let yaml = GATING.replace(
            "  pull_request:\n    branches: [main]\n",
            "  push:\n    branches: [main]\n",
        );
        assert!(
            gating_runs_that_prove_the_build_traps(&yaml).is_empty(),
            "scoping the scan to ci.yml assumes ci.yml is what runs on a pull request; if its \
             triggers stop including pull_request, the step gates nothing no matter how it looks"
        );
    }

    /// A `run: |` block is read whole, so a command inside a loop is
    /// visible. This repository has TWO such loops in kernel-gate, and
    /// a line-range extraction drops the second.
    #[test]
    fn a_run_block_is_read_whole() {
        let yaml = "\
on:
  pull_request:
    branches: [main]
jobs:
  test:
    steps:
      - name: a block
        run: |
          set -euo pipefail
          EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib
";
        assert_eq!(
            gating_runs_that_prove_the_build_traps(yaml).len(),
            1,
            "a command inside a `run: |` block must be seen; the kernel-gate loops live in \
             blocks like this one"
        );
    }

    /// THE QUOTED SPELLINGS, WHICH WERE SILENT DEFEATS. Measured on
    /// `main` at `57cf1b6`: `if: false` correctly turned the suite red,
    /// and `"if": false` -- the same key, quoted -- left all 34 tests
    /// green while Actions skipped the step. The old parser took its
    /// key as `cur.split(':').next()` with no un-quoting, so the key
    /// read `"if"` and matched no entry in `NON_GATING_KEYS`.
    ///
    /// Nothing un-quotes anything now: the key arrives from the parser
    /// already resolved, so every spelling of it is the same key by
    /// construction.
    #[test]
    fn a_quoted_key_is_the_same_key() {
        for spelling in [
            "\"if\": false",
            "'if': false",
            "\"continue-on-error\": true",
            "'continue-on-error': true",
        ] {
            let yaml = GATING.replace(
                "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n",
                &format!(
                    "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n        {spelling}\n"
                ),
            );
            assert!(
                gating_runs_that_prove_the_build_traps(&yaml).is_empty(),
                "`{spelling}` is the same key as its bare spelling; quoting it must not \
                 make a skipped step count as the thing gating the merge"
            );
        }
    }

    /// And one level up, on the job.
    #[test]
    fn a_quoted_key_on_the_job_is_the_same_key() {
        for spelling in ["\"if\": false", "\"continue-on-error\": true"] {
            let yaml = GATING.replace("  test:\n", &format!("  test:\n    {spelling}\n"));
            assert!(
                gating_runs_that_prove_the_build_traps(&yaml).is_empty(),
                "`{spelling}` on the job is the same key as its bare spelling"
            );
        }
    }

    /// THE COMMENTED-OUT TRIGGER, ALSO A SILENT DEFEAT. The old check
    /// asked whether the `on:` block's raw text -- comments included --
    /// contained the characters `pull_request`, so commenting the
    /// trigger out left the guard green on a workflow that no longer
    /// ran on pull requests at all. Measured on `main`: 34 passed,
    /// both arms.
    #[test]
    fn a_commented_out_pull_request_trigger_does_not_gate() {
        let commented_with_another_trigger_left = GATING.replace(
            "  pull_request:\n    branches: [main]\n",
            "  # pull_request:\n  #   branches: [main]\n  push:\n    branches: [main]\n",
        );
        let only_a_comment_naming_it = GATING.replace(
            "  pull_request:\n    branches: [main]\n",
            "  # pull_request disabled while we investigate flaky runners\n  push:\n    branches: [main]\n",
        );
        for yaml in [
            &commented_with_another_trigger_left,
            &only_a_comment_naming_it,
        ] {
            assert!(
                gating_runs_that_prove_the_build_traps(yaml).is_empty(),
                "a trigger named only in a comment is not a trigger; the parser drops \
                 comments before anything compares a name, so there is no `#` to strip \
                 and none to forget:\n{yaml}"
            );
        }
    }

    /// A whole-name comparison, so a trigger that merely begins with
    /// those characters is a different trigger. `pull_request_review`
    /// fires on a review, not on the pull request, and cannot be what
    /// gates the merge.
    #[test]
    fn a_trigger_that_merely_begins_with_pull_request_does_not_gate() {
        let yaml = GATING.replace(
            "  pull_request:\n    branches: [main]\n",
            "  pull_request_review:\n    types: [submitted]\n",
        );
        assert!(
            gating_runs_that_prove_the_build_traps(&yaml).is_empty(),
            "pull_request_review is not pull_request; a substring match cannot tell \
             them apart and this comparison must"
        );
    }

    /// `pull_request_target` is not `pull_request`, and is refused on
    /// purpose. It runs against the base repository with a write token
    /// and the repository's secrets, and checks out the base ref by
    /// default, so a workflow triggered only that way may never build
    /// the contributor's code. `rust-fs-xfs#146` and
    /// `rust-fs-ext4#149` record it as a live gap in the hand-rolled
    /// guard this file replaces.
    ///
    /// Pinned as a test rather than left to the comparison, because the
    /// clause is one line and was previously written by hand and copied
    /// between repositories. This is what stops it coming back.
    #[test]
    fn pull_request_target_does_not_gate() {
        let yaml = GATING.replace(
            "  pull_request:\n    branches: [main]\n",
            "  pull_request_target:\n    branches: [main]\n",
        );
        assert!(
            gating_runs_that_prove_the_build_traps(&yaml).is_empty(),
            "pull_request_target runs with the base repository's token and secrets \
             and checks out the base ref; it is not proof that the merge is gated"
        );
    }

    /// `on:` may be a sequence of names rather than a mapping, in
    /// either the flow or the block spelling, and all three are
    /// ordinary workflows.
    #[test]
    fn a_sequence_of_triggers_is_read() {
        for spelling in [
            "on: [push, pull_request]\n",
            "on:\n  - push\n  - pull_request\n",
        ] {
            let yaml = GATING.replace("on:\n  pull_request:\n    branches: [main]\n", spelling);
            assert_eq!(
                gating_runs_that_prove_the_build_traps(&yaml).len(),
                1,
                "this workflow triggers on a pull request as surely as the mapping \
                 spelling does:\n{yaml}"
            );
        }
    }

    /// THE ARM THAT WAS A FALSE ALARM RATHER THAN A DEFEAT, AND SO
    /// CANNOT BE WITNESSED BY THE SUITE GOING RED -- it already did.
    /// The witness is that legal YAML now passes.
    ///
    /// The old parser treated only a bare `|` as a block opener
    /// (`after != "|"`), so `|-`, `|+`, `>`, `>-` and `|2` were read as
    /// the command itself and the block's contents never parsed at all.
    /// Measured on `main`: `run: |` 34 passed, `run: |-` and `run: >`
    /// each EXIT=101 with 2 failed -- the guard refusing a completely
    /// correct workflow, which is the fastest way to get a guard
    /// deleted.
    ///
    /// A parser knows all five styles because they are the grammar.
    #[test]
    fn every_block_scalar_style_is_read_whole() {
        for style in ["|", "|-", "|+", ">", ">-", "|2"] {
            let yaml = GATING.replace(
                "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n",
                &format!(
                    "      - name: a block\n        run: {style}\n          EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n"
                ),
            );
            assert_eq!(
                gating_runs_that_prove_the_build_traps(&yaml).len(),
                1,
                "`run: {style}` is a legal block scalar carrying the gating command; \
                 failing here is the guard refusing a correct workflow:\n{yaml}"
            );
        }
    }

    /// A command quoted in a YAML comment is not a run. This used to be
    /// the shell scanner's job and is the parser's now: comments do not
    /// survive parsing, so there is no `#` handling here to get wrong.
    /// It is asserted at this level because that is where the property
    /// now lives -- `ci.yml` really does quote the gating command
    /// verbatim in the comment block above it, so a scan that missed
    /// this would stay green after the step itself was deleted.
    #[test]
    fn a_debug_run_quoted_in_a_yaml_comment_does_not_gate() {
        let yaml = "\
on:
  pull_request:
    branches: [main]
jobs:
  test:
    steps:
      # Do not remove this as a duplicate of the runs above it:
      #     - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib
      - run: cargo test --locked --release
";
        assert!(
            gating_runs_that_prove_the_build_traps(yaml).is_empty(),
            "the gating command appears only inside a comment, and the step that \
             remains is a release run"
        );
    }

    /// A workflow the parser cannot read is a failure, never a pass.
    /// The direction matters: a guard that swallowed the error and
    /// returned an empty structure would report "no debug run gates
    /// this", which is also a failure and therefore safe -- but one
    /// that returned early with a pass would be the blindness this
    /// whole module exists to refuse.
    #[test]
    #[should_panic(expected = "not valid YAML")]
    fn a_workflow_that_does_not_parse_is_a_failure() {
        super::parse_workflow("jobs:\n  test:\n   - broken: [unclosed\n");
    }

    /// THE CONTROL THAT STOPS THE REFUSAL OVER-CORRECTING.
    ///
    /// `pull_request_target` is refused as insufficient ON ITS OWN.
    /// That is not the same as refusing any workflow that mentions it,
    /// and until this test existed nothing in the file could tell the
    /// two apart: every fixture carried at most one trigger, so this
    /// mutation survived the whole suite --
    ///
    /// ```text
    ///   any(t == "pull_request")
    ///       && !any(t == "pull_request_target")
    /// ```
    ///
    /// -- while refusing a perfectly gated workflow. Carrying both
    /// triggers is the ordinary way to reach repository secrets from a
    /// job without giving up the pull-request gate, and such a workflow
    /// IS gated, by its `pull_request:` key.
    ///
    /// An assertion whose result does not depend on the thing it claims
    /// to check is this project's own recurring defect; this one was in
    /// the test pinning the refusal rather than in the refusal itself.
    #[test]
    fn a_workflow_carrying_both_triggers_still_gates() {
        let yaml = GATING.replace(
            "  pull_request:\n    branches: [main]\n",
            "  pull_request:\n    branches: [main]\n  pull_request_target:\n    branches: [main]\n",
        );
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert_eq!(
            gating_runs_that_prove_the_build_traps(&yaml).len(),
            1,
            "the workflow still triggers on pull_request, so it still gates; refusing it \
             because pull_request_target is also present would be the over-correction"
        );
    }

    /// THE HANDSHAKE MAY BE DECLARED IN THE STEP'S `env:` MAPPING.
    ///
    /// Not a concession: on a matrix including `windows-latest` it is
    /// the only spelling that works, because an inline
    /// `VAR=1 cargo test` prefix is bash syntax and a PowerShell syntax
    /// error. A guard that read only the command would refuse the
    /// correct workflow on every cross-platform crate here -- the loud
    /// direction, but wrong, and the fastest way to get a guard
    /// deleted.
    #[test]
    fn the_handshake_declared_in_an_env_mapping_counts() {
        for value in ["\"1\"", "1", "'1'"] {
            let yaml = GATING.replace(
                "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n",
                &format!(
                    "      - run: cargo test --locked --lib\n        env:\n          EXPECT_OVERFLOW_CHECKS: {value}\n"
                ),
            );
            assert_eq!(
                gating_runs_that_prove_the_build_traps(&yaml).len(),
                1,
                "`EXPECT_OVERFLOW_CHECKS: {value}` in an env mapping is the same \
                 instruction to Actions as the inline prefix, and on a Windows \
                 matrix it is the only one that works:\n{yaml}"
            );
        }
    }

    /// And it must not rescue a `--release` run. The checks are off in
    /// release deliberately, so a handshake there arms an assertion
    /// that would fire on every green run.
    #[test]
    fn the_handshake_in_an_env_mapping_does_not_count_on_a_release_run() {
        let yaml = GATING.replace(
            "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n",
            "      - run: cargo test --locked --release --lib\n        env:\n          EXPECT_OVERFLOW_CHECKS: \"1\"\n",
        );
        assert!(
            gating_runs_that_prove_the_build_traps(&yaml).is_empty(),
            "a release run cannot prove the build traps, however it is labelled"
        );
    }

    /// A step carrying an `env:` mapping is still a gating step. Over-
    /// strictness here would cost something real: the handshake itself
    /// lives in such a mapping.
    #[test]
    fn a_step_carrying_an_env_mapping_still_gates() {
        let yaml = GATING.replace(
            "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n",
            "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n        env:\n          SOMETHING_ELSE: \"1\"\n",
        );
        assert_eq!(
            gating_runs_that_prove_the_build_traps(&yaml).len(),
            1,
            "`env:` says nothing about whether the step's result is read"
        );
    }
}
