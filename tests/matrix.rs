// matrix.rs — data-driven NTFS scenario runner.
//
// Each scenario in test-matrix.json becomes one libtest-mimic
// trial. The trial body delegates the Windows-side lifecycle to
// scripts/run-scenario.ps1 (which produces the byte-diff evidence
// packet); this file owns:
//   1. scenario filtering (phase-1 = mac:format → win:chkdsk*)
//   2. rust-ntfs format invocation
//   3. evidence-packet bookkeeping: per-scenario manifest.json +
//      result.json, run-level results.json + run-manifest.json
//
// The diag layout (under diag/matrix/) is the contract an automated
// fix-loop reads; see scripts/run-scenario.ps1's header for the file
// list. results.json + per-scenario result.json are the two files an
// agent grep's first.
//
// On non-Windows the trial is marked ignored — the runner is portable
// (so cargo check / cargo test on Linux doesn't spuriously fail) but
// only does useful work on a Windows host.

use libtest_mimic::{Arguments, Failed, Trial};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

// VHDX mount uses Windows-global state (drive letters, disk numbers).
// Serialise mount/chkdsk/dismount across trials. mkfs runs in parallel
// because each scenario gets a distinct .img path.
static MOUNT_LOCK: Mutex<()> = Mutex::new(());

// --verbose / MATRIX_VERBOSE=1 turns on the live per-step tree printed
// from each trial body. Every op in operation_sequence becomes one line
// inside the trial: the description prints first (no newline) and the
// `OK` / `FAIL` status appends to the same line once the op finishes.
// libtest-mimic's stderr capture is disabled (args.nocapture = true) so
// the lines actually reach the terminal as the trial runs.
static VERBOSE: AtomicBool = AtomicBool::new(false);

// Per-trial verbose context. Tracks how many steps have been emitted so
// the ASCII connector flips to `└──` for the final step.
struct Verbose {
    enabled: bool,
    step_idx: usize,
    total: usize,
}

impl Verbose {
    fn new(scn: &Scenario) -> Self {
        let total = scn
            .operation_sequence
            .split("->")
            .filter(|s| !s.trim().is_empty())
            .count();
        Self {
            enabled: VERBOSE.load(Ordering::Relaxed),
            step_idx: 0,
            total,
        }
    }

    fn header(&self, name: &str, summary: &str) {
        if !self.enabled {
            return;
        }
        eprintln!();
        eprintln!("• {name}");
        eprintln!("  scenario: {summary}");
    }

    fn connector(&self) -> &'static str {
        if self.step_idx + 1 == self.total {
            "└──"
        } else {
            "├──"
        }
    }

    // Step kicks off: print description, no newline, flush so the user
    // sees it before the actual work begins.
    fn step_start(&self, description: &str) {
        if !self.enabled {
            return;
        }
        use std::io::Write;
        let c = self.connector();
        eprint!("  {c} we are testing: {description} ... ");
        let _ = std::io::stderr().flush();
    }

    fn step_ok(&mut self) {
        if self.enabled {
            eprintln!("OK");
        }
        self.step_idx += 1;
    }

    fn step_fail(&mut self, reason: &str) {
        if self.enabled {
            eprintln!("FAIL ({reason})");
        }
        self.step_idx += 1;
    }

    // For batched ops (e.g. win:* verbs handled in one PS invocation)
    // there's no live time gap between description and result, so we
    // emit both halves on a single eprintln.
    fn step_inline_ok(&mut self, description: &str) {
        if self.enabled {
            let c = self.connector();
            eprintln!("  {c} we are testing: {description} ... OK");
        }
        self.step_idx += 1;
    }

    fn step_inline_fail(&mut self, description: &str, reason: &str) {
        if self.enabled {
            let c = self.connector();
            eprintln!("  {c} we are testing: {description} ... FAIL ({reason})");
        }
        self.step_idx += 1;
    }

    fn footer(&self, status: &str, detail: Option<&str>) {
        if !self.enabled {
            return;
        }
        match detail {
            Some(d) => eprintln!("  => {status} ({d})"),
            None => eprintln!("  => {status}"),
        }
    }
}

#[derive(Deserialize)]
struct WorkList {
    scenarios: std::collections::BTreeMap<String, Scenario>,
}

#[derive(Deserialize, Clone)]
struct Scenario {
    volume_params: VolumeParams,
    operation_sequence: String,
    /// Optional fixture-file recipe applied by run-scenario.ps1 after
    /// mount, before chkdsk. Absence means no win-side writes.
    #[serde(default)]
    fixture_files: Vec<FixtureFile>,
    /// Verdict shape — controls how chkdsk /F (Stage E2) interacts
    /// with pass/fail. Defaults to `Clean` for backwards compat with
    /// legacy scenarios.
    ///
    /// - `clean` — must pass without /F running at all.
    /// - `repair-ok` — passes whether or not /F ran, as long as
    ///   post-/F /scan is clean if it did.
    /// - `repair-required` — /F MUST run AND post-/F /scan must
    ///   exit 0. Used by dirty-volume Tier-3 scenarios where /F
    ///   doing real work is the test contract.
    #[serde(default)]
    verdict_shape: VerdictShape,
    // Other fields (status, evidence_link, _attempts, ...) are
    // intentionally ignored — they're agent bookkeeping, not test input.
}

#[derive(Deserialize, Clone, Copy, Default, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum VerdictShape {
    #[default]
    Clean,
    RepairOk,
    RepairRequired,
}

#[derive(Deserialize, Serialize, Clone)]
struct FixtureFile {
    name: String,
    /// Inline UTF-8 content. Set OR `size_bytes`+`content_pattern`,
    /// not both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size_bytes: Option<u64>,
    /// One of: "zeros", "ones", "incrementing", "random".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content_pattern: Option<String>,
}

#[derive(Deserialize, Serialize, Clone)]
struct VolumeParams {
    size_mib: u64,
    cluster_size: u32,
    label: String,
}

// Per-scenario manifest written before the test body runs. Lets an
// agent walking into a diag dir cold reconstruct what was tested.
#[derive(Serialize)]
struct ScenarioManifest<'a> {
    name: &'a str,
    operation_sequence: &'a str,
    volume_params: &'a VolumeParams,
    runner: &'static str,
    runner_version: &'static str,
    timestamp_utc: String,
}

// Per-scenario verdict emitted regardless of pass/fail. Aggregated into
// run-level results.json after libtest-mimic completes.
#[derive(Serialize, Deserialize, Clone)]
struct ScenarioResult {
    name: String,
    status: String, // "passed" | "failed" | "errored"
    ro_exit: Option<i32>,
    scan_exit: Option<i32>,
    error: Option<String>,
    diag_dir: String,
    duration_secs: f64,
}

// Run-level aggregate written once after all trials have run.
#[derive(Serialize)]
struct RunManifest {
    timestamp_utc: String,
    host_os: &'static str,
    git_sha: Option<String>,
    scenario_count_total: usize,
    scenario_count_runnable: usize,
}

fn main() {
    // Strip our custom --verbose flag before libtest-mimic parses argv
    // (clap inside libtest-mimic would otherwise reject the unknown
    // flag). MATRIX_VERBOSE=1 in the env is an equivalent toggle for
    // CI / wrapper scripts that don't want to fight argv ordering.
    let mut argv: Vec<String> = std::env::args().collect();
    let mut idx = 1;
    while idx < argv.len() {
        if argv[idx] == "--verbose" {
            VERBOSE.store(true, Ordering::Relaxed);
            argv.remove(idx);
        } else {
            idx += 1;
        }
    }
    if let Ok(v) = std::env::var("MATRIX_VERBOSE") {
        if v == "1" || v.eq_ignore_ascii_case("true") {
            VERBOSE.store(true, Ordering::Relaxed);
        }
    }

    let mut args = Arguments::from_iter(argv);
    if VERBOSE.load(Ordering::Relaxed) {
        // Force libtest-mimic to stream stderr from trial bodies so the
        // per-scenario tree we print from run_scenario actually reaches
        // the terminal (otherwise libtest-mimic buffers and only emits
        // captured output for failing trials).
        args.nocapture = true;
    }

    let worklist_path = workspace_root().join("test-matrix.json");
    let raw = std::fs::read_to_string(&worklist_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", worklist_path.display()));
    let wl: WorkList = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("parse {}: {e}", worklist_path.display()));

    let total = wl.scenarios.len();
    let mut runnable = 0usize;

    let trials: Vec<Trial> = wl
        .scenarios
        .into_iter()
        .map(|(name, scn)| {
            let runnable_op = is_runnable(&scn.operation_sequence);
            let pure_mac = is_pure_mac_chain(&scn.operation_sequence);
            if runnable_op {
                runnable += 1;
            }
            let body_name = name.clone();
            let trial = Trial::test(name, move || run_scenario(&body_name, &scn));
            // Pure-mac chains run on any host. Windows-side scenarios
            // need a Windows host to drive VHDX mount + chkdsk.
            let needs_windows = !pure_mac;
            if !runnable_op || (needs_windows && !cfg!(target_os = "windows")) {
                trial.with_ignored_flag(true)
            } else {
                trial
            }
        })
        .collect();

    // Wipe any prior run's diag tree so this run's results are
    // self-contained — otherwise the local scaffold pulls a mix of
    // fresh + stale per-scenario dirs and an automated fix-loop has
    // to date-discriminate. Skip when --list (no trials will run).
    if !args.list {
        let _ = std::fs::remove_dir_all(matrix_diag_root());
    }

    // run-manifest.json is written before trials so it exists even if
    // every trial panics — useful for an agent that only finds a
    // partial diag tree.
    let _ = write_run_manifest(total, runnable);

    let conclusion = libtest_mimic::run(&args, trials);

    // Aggregate per-scenario result.json files into results.json.
    // Done after run() returns (libtest-mimic's run is synchronous and
    // joins all threads before returning the Conclusion).
    if !args.list {
        let _ = aggregate_results();
    }

    conclusion.exit();
}

// Three runnable shapes today:
//   1. Pure-mac chains: every op is mac:* (format/touch/mkdir/write/rm/
//      rmdir/enumerate/set-dirty). Dispatched via the rust-ntfs binary,
//      no Windows VHDX involvement.
//   2. Mac-prefix + win-chkdsk-suffix: leading mac: ops are dispatched
//      by us; the trailing win:chkdsk*/win:enumerate ops are handled
//      by scripts/run-scenario.ps1 on a Windows host.
//   3. Legacy phase-1 (`mac:format -> win:chkdsk*`) — a special case
//      of (2).
// Anything else (win:format, win:write/delete/modify, mac ops AFTER a
// win: op) is reported as ignored until the win-side dispatcher grows
// to handle the additional verbs.
fn is_runnable(sequence: &str) -> bool {
    let s = sequence.trim();
    if is_pure_mac_chain(s) {
        return true;
    }
    let ops: Vec<&str> = s
        .split("->")
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .collect();
    let win_idx = ops
        .iter()
        .position(|op| op.starts_with("win:"))
        .unwrap_or(ops.len());
    let (mac_prefix, win_suffix) = ops.split_at(win_idx);

    // Mac prefix: must begin with mac:format; every verb known to the
    // dispatcher.
    if mac_prefix.is_empty() || !mac_prefix[0].starts_with("mac:format") {
        return false;
    }
    let known_mac = [
        "mac:format",
        "mac:mkdir",
        "mac:touch",
        "mac:write",
        "mac:rm",
        "mac:rmdir",
        "mac:enumerate",
        "mac:set-dirty",
    ];
    for op in mac_prefix {
        let verb = op.split('(').next().unwrap_or(op).trim();
        if !known_mac.contains(&verb) {
            return false;
        }
    }

    // Win suffix: chkdsk / enumerate / repeat-mount handled directly.
    // win:write / win:delete are folded in via the FixturesJson PS
    // parameter. mac:* ops are allowed AFTER win: ops too — they run
    // post-Windows on the extracted partition image (Stage F.5 of the
    // PS script). Reject anything else.
    for op in win_suffix {
        let ok = op.starts_with("win:chkdsk")
            || op.starts_with("win:enumerate")
            || op.starts_with("win:repeat-mount")
            || op.starts_with("win:write")
            || op.starts_with("win:delete")
            || op.starts_with("win:dismount")
            || op.starts_with("win:remount")
            || {
                let verb = op.split('(').next().unwrap_or(op).trim();
                known_mac.contains(&verb)
            };
        if !ok {
            return false;
        }
    }
    true
}

fn is_pure_mac_chain(sequence: &str) -> bool {
    !sequence.contains("win:")
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn matrix_diag_root() -> PathBuf {
    workspace_root().join("diag/matrix")
}

fn run_scenario(name: &str, scn: &Scenario) -> Result<(), Failed> {
    let pure_mac = is_pure_mac_chain(&scn.operation_sequence);
    if !pure_mac && !cfg!(target_os = "windows") {
        return Err(Failed::from(
            "non-Windows host — should have been ignored (mixed mac/win sequence needs a Windows host)",
        ));
    }

    let started = std::time::Instant::now();
    let workdir = workspace_root();
    let diag = matrix_diag_root().join(name);
    std::fs::create_dir_all(&diag).map_err(|e| f(format!("mkdir diag: {e}")))?;

    // Manifest first — even if we fail mid-trial, the diag dir tells
    // an agent what was being attempted.
    write_scenario_manifest(name, scn, &diag);

    let img = workdir.join(format!("nfs-{name}.img"));
    let vhdx = workdir.join(format!("wrapper-{name}.vhdx"));
    let reference_vhdx = workdir.join(format!("reference-{name}.vhdx"));

    // Best-effort cleanup of any leftover artefacts.
    let _ = std::fs::remove_file(&img);
    let _ = std::fs::remove_file(&vhdx);
    let _ = std::fs::remove_file(&reference_vhdx);

    let mut v = Verbose::new(scn);
    v.header(name, &scenario_summary(scn));

    let outcome = run_scenario_inner(
        name,
        scn,
        &workdir,
        &diag,
        &img,
        &vhdx,
        &reference_vhdx,
        &mut v,
    );
    let elapsed = started.elapsed().as_secs_f64();

    match &outcome {
        Ok((ro, scan)) => v.footer("PASSED", Some(&format!("ro={ro} scan={scan}"))),
        Err(ScenarioFailure::ChkdskFail { ro, scan }) => {
            v.footer("FAILED", Some(&format!("ro={ro} scan={scan}")))
        }
        Err(ScenarioFailure::Errored(m)) => v.footer("ERRORED", Some(m)),
    }

    // Always write result.json — pass, fail, or error all share the
    // same on-disk schema so the aggregator doesn't need special cases.
    let result = match &outcome {
        Ok((ro, scan)) => ScenarioResult {
            name: name.to_string(),
            status: "passed".into(),
            ro_exit: Some(*ro),
            scan_exit: Some(*scan),
            error: None,
            diag_dir: diag.display().to_string(),
            duration_secs: elapsed,
        },
        Err(ScenarioFailure::ChkdskFail { ro, scan }) => ScenarioResult {
            name: name.to_string(),
            status: "failed".into(),
            ro_exit: Some(*ro),
            scan_exit: Some(*scan),
            error: Some(format!(
                "chkdsk exit codes ro={ro} scan={scan} (expected 0/0)"
            )),
            diag_dir: diag.display().to_string(),
            duration_secs: elapsed,
        },
        Err(ScenarioFailure::Errored(msg)) => ScenarioResult {
            name: name.to_string(),
            status: "errored".into(),
            ro_exit: None,
            scan_exit: None,
            error: Some(msg.clone()),
            diag_dir: diag.display().to_string(),
            duration_secs: elapsed,
        },
    };
    let _ = std::fs::write(
        diag.join("result.json"),
        serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".into()),
    );

    match outcome {
        Ok(_) => Ok(()),
        Err(ScenarioFailure::ChkdskFail { ro, scan }) => Err(f(format!(
            "chkdsk exit codes ro={ro} scan={scan} (expected 0/0); diag at {}",
            diag.display()
        ))),
        Err(ScenarioFailure::Errored(msg)) => Err(f(msg)),
    }
}

enum ScenarioFailure {
    ChkdskFail { ro: i32, scan: i32 },
    Errored(String),
}

#[allow(clippy::too_many_arguments)]
fn run_scenario_inner(
    _name: &str,
    scn: &Scenario,
    workdir: &Path,
    diag: &Path,
    img: &Path,
    vhdx: &Path,
    reference_vhdx: &Path,
    v: &mut Verbose,
) -> Result<(i32, i32), ScenarioFailure> {
    // 1. Allocate blank raw image.
    let f_img = std::fs::File::create(img)
        .map_err(|e| ScenarioFailure::Errored(format!("create img: {e}")))?;
    f_img
        .set_len(scn.volume_params.size_mib * 1024 * 1024)
        .map_err(|e| ScenarioFailure::Errored(format!("set_len: {e}")))?;
    drop(f_img);

    // Pure-mac chains: dispatch every op through the rust-ntfs binary
    // and skip the VHDX/PowerShell leg entirely. Returns (0, 0) on
    // success so the existing chkdsk-shaped result schema still fits.
    if is_pure_mac_chain(&scn.operation_sequence) {
        run_mac_ops(scn, img, diag, v)?;
        return Ok((0, 0));
    }

    // Mixed sequences: split into mac-prefix and win-suffix at the
    // first `win:` op. Run every mac op through the rust-ntfs CLI;
    // hand the formatted image off to scripts/run-scenario.ps1 for
    // the win-suffix (VHDX wrap/mount/chkdsk + optional repeat-mount).
    let ops: Vec<&str> = scn
        .operation_sequence
        .split("->")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let win_idx = ops
        .iter()
        .position(|op| op.starts_with("win:"))
        .unwrap_or(ops.len());
    let (mac_prefix, win_suffix) = ops.split_at(win_idx);

    // Tier-3 repeat-mount: scenarios encode the cycle count via
    // `win:repeat-mount(N)`. The PS script accepts -RemountCycles N;
    // pull the count out here and forward it.
    let remount_cycles: i32 = win_suffix
        .iter()
        .filter_map(|op| {
            op.strip_prefix("win:repeat-mount")
                .and_then(|tail| tail.trim().strip_prefix('('))
                .and_then(|t| t.strip_suffix(')'))
                .and_then(|n| n.trim().parse::<i32>().ok())
        })
        .sum();

    // win:write fixtures: serialise scenario.fixture_files to JSON for
    // the PS script to apply after mount. Empty array if the scenario
    // doesn't declare any.
    let fixtures_path = diag.join("fixtures.json");
    let _ = std::fs::write(
        &fixtures_path,
        serde_json::to_string_pretty(&scn.fixture_files).unwrap_or_else(|_| "[]".into()),
    );

    let bin = rust_ntfs_path();
    for (i, raw) in mac_prefix.iter().enumerate() {
        let desc = describe_op(raw, scn);
        v.step_start(&desc);
        match run_one_mac_op(raw, &bin, img, diag, i + 1, scn) {
            Ok(()) => v.step_ok(),
            Err(e) => {
                let reason = scenario_failure_msg(&e);
                v.step_fail(&reason);
                return Err(ScenarioFailure::Errored(format!(
                    "mac-prefix step {} ({raw}): {reason}",
                    i + 1
                )));
            }
        }
    }

    // 3-5. Wrap → mount → reference-format → chkdsk → event-log.
    // Serialised across trials because Windows drive-letter assignment
    // is process-global. See scripts/run-scenario.ps1 for details.
    let _guard = MOUNT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let ps_script = workdir.join("scripts/run-scenario.ps1");
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&ps_script)
        .args(["-Img", &img.display().to_string()])
        .args(["-Vhdx", &vhdx.display().to_string()])
        .args(["-ReferenceVhdx", &reference_vhdx.display().to_string()])
        .args(["-Diag", &diag.display().to_string()])
        .args(["-Label", &scn.volume_params.label])
        .args(["-ClusterSize", &scn.volume_params.cluster_size.to_string()])
        .args(["-VolumeSizeMb", &scn.volume_params.size_mib.to_string()])
        .args(["-RemountCycles", &remount_cycles.to_string()])
        .args(["-FixturesJson", &fixtures_path.display().to_string()])
        .output()
        .map_err(|e| ScenarioFailure::Errored(format!("spawn run-scenario.ps1: {e}")))?;

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let _ = std::fs::write(diag.join("ps-stdout.txt"), &stdout);
    let _ = std::fs::write(diag.join("ps-stderr.txt"), &stderr);

    if !out.status.success() {
        let reason = format!("run-scenario.ps1 exit {:?}", out.status.code());
        for raw in &ops[win_idx..] {
            if raw.starts_with("win:") {
                v.step_inline_fail(&describe_op(raw, scn), &reason);
            }
        }
        return Err(ScenarioFailure::Errored(format!(
            "{reason}: {}",
            stderr.trim()
        )));
    }

    let ro = parse_marker(&stdout, "RO_EXIT=").ok_or_else(|| {
        ScenarioFailure::Errored(format!("RO_EXIT not found in PS output:\n{stdout}"))
    })?;
    let scan = parse_marker(&stdout, "SCAN_EXIT=").ok_or_else(|| {
        ScenarioFailure::Errored(format!("SCAN_EXIT not found in PS output:\n{stdout}"))
    })?;

    // TODO(/scan-13-ceiling): tighten to `scan == 0` once the byte that
    // makes Microsoft `format.com`'s output pass /scan but ours fail is
    // pinned down. See `docs/FUTURE_FEATURES.md` "/scan exit 13 ceiling"
    // and `docs/overnight-findings.md` iter G for the full investigation
    // (chkdsk /F runs cleanly + post-/F /scan exits 0, so the volume IS
    // structurally sound; reference passes /scan from a structurally-
    // identical volume).
    //
    // Pass criteria today:
    //   * `chkdsk readonly` MUST exit 0 — ntfs.sys mounts and reads
    //     every record without flagging corruption (the user-facing
    //     "is this a valid NTFS volume" contract).
    //   * `chkdsk /scan` MAY exit 0, 11, or 13:
    //       - 0:  no problems found (ideal — what we want to require).
    //       - 11: VSS shadow-storage allocation fails on volumes
    //             smaller than ~33 MiB (orthogonal to filesystem state).
    //       - 13: "errors queued for offline repair" — chkdsk /F finds
    //             nothing to fix, post-/F /scan exits 0. Documented
    //             technical debt, not a real corruption.
    //   * Any other exit code = real corruption, fail.
    let scan_ok = matches!(scan, 0 | 11 | 13);
    let fix_exit = parse_marker(&stdout, "FIX_EXIT=");
    let postfix_scan_exit = parse_marker(&stdout, "POSTFIX_SCAN_EXIT=");

    // Per-shape verdict logic. Default (Clean) preserves the existing
    // chkdsk-must-pass contract; RepairOk widens to "fine if /F
    // succeeded"; RepairRequired flips it: /F MUST run and the
    // post-/F /scan MUST exit 0.
    let scenario_passed = match scn.verdict_shape {
        VerdictShape::Clean => ro == 0 && scan_ok,
        VerdictShape::RepairOk => {
            let pre_clean = ro == 0 && scan_ok;
            let post_clean = matches!(fix_exit, Some(0))
                && matches!(postfix_scan_exit, Some(0) | Some(11) | Some(13));
            pre_clean || post_clean
        }
        VerdictShape::RepairRequired => {
            // /F must have actually run (Stage E2 only runs when
            // pre-/F /scan returned non-zero) AND succeeded, AND
            // post-/F /scan must exit 0.
            let f_ran = fix_exit.is_some();
            let f_ok = matches!(fix_exit, Some(0));
            let post_ok = matches!(postfix_scan_exit, Some(0));
            f_ran && f_ok && post_ok
        }
    };

    // Record outcomes for every win:* op in the suffix. PS runs the
    // win ops as a single batch, so non-chkdsk verbs (repeat-mount,
    // enumerate, dismount, remount, write, delete) inherit "Pass" from
    // PS exiting 0; chkdsk verbs gate on the per-shape verdict above.
    for raw in &ops[win_idx..] {
        if !raw.starts_with("win:") {
            continue;
        }
        let desc = describe_op(raw, scn);
        if raw.starts_with("win:chkdsk") {
            if scenario_passed {
                v.step_inline_ok(&desc);
            } else {
                v.step_inline_fail(&desc, &format!("ro={ro} scan={scan}"));
            }
        } else {
            v.step_inline_ok(&desc);
        }
    }

    if !scenario_passed {
        return Err(ScenarioFailure::ChkdskFail { ro, scan });
    }

    // Post-Windows mac ops: any mac:* ops AFTER the first win: op run
    // against the partition contents extracted by Stage F.5 of the PS
    // script (suffix `.post.img`). The PS script writes the post image
    // before its final dismount; if it isn't present, treat as an
    // infrastructure error rather than silently passing.
    let mac_suffix: Vec<&str> = ops[win_idx..]
        .iter()
        .copied()
        .filter(|op| op.starts_with("mac:"))
        .collect();
    if !mac_suffix.is_empty() {
        let post_img = workdir.join(format!(
            "{}.post.img",
            img.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("nfs.img")
        ));
        if !post_img.is_file() {
            return Err(ScenarioFailure::Errored(format!(
                "post-windows mac ops require {} but the PS script did not produce it (Stage F.5 may have failed)",
                post_img.display()
            )));
        }
        for (i, raw) in mac_suffix.iter().enumerate() {
            let desc = describe_op(raw, scn);
            v.step_start(&desc);
            match run_one_mac_op(raw, &bin, &post_img, diag, 100 + i + 1, scn) {
                Ok(()) => v.step_ok(),
                Err(e) => {
                    let reason = scenario_failure_msg(&e);
                    v.step_fail(&reason);
                    return Err(ScenarioFailure::Errored(format!(
                        "post-win mac step {} ({raw}): {reason}",
                        i + 1
                    )));
                }
            }
        }
    }

    Ok((ro, scan))
}

fn parse_marker(s: &str, prefix: &str) -> Option<i32> {
    s.lines()
        .find_map(|l| l.trim().strip_prefix(prefix))
        .and_then(|v| v.trim().parse().ok())
}

fn write_scenario_manifest(name: &str, scn: &Scenario, diag: &Path) {
    let manifest = ScenarioManifest {
        name,
        operation_sequence: &scn.operation_sequence,
        volume_params: &scn.volume_params,
        runner: "tests/matrix.rs",
        runner_version: env!("CARGO_PKG_VERSION"),
        timestamp_utc: now_iso8601(),
    };
    let _ = std::fs::write(
        diag.join("manifest.json"),
        serde_json::to_string_pretty(&manifest).unwrap_or_else(|_| "{}".into()),
    );
}

fn write_run_manifest(total: usize, runnable: usize) -> std::io::Result<()> {
    let root = matrix_diag_root();
    std::fs::create_dir_all(&root)?;
    let manifest = RunManifest {
        timestamp_utc: now_iso8601(),
        host_os: std::env::consts::OS,
        git_sha: git_sha(),
        scenario_count_total: total,
        scenario_count_runnable: runnable,
    };
    std::fs::write(
        root.join("run-manifest.json"),
        serde_json::to_string_pretty(&manifest).unwrap_or_else(|_| "{}".into()),
    )
}

fn aggregate_results() -> std::io::Result<()> {
    let root = matrix_diag_root();
    let mut results: Vec<ScenarioResult> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let p = entry.path().join("result.json");
            if p.is_file() {
                if let Ok(raw) = std::fs::read_to_string(&p) {
                    if let Ok(r) = serde_json::from_str::<ScenarioResult>(&raw) {
                        results.push(r);
                    }
                }
            }
        }
    }
    results.sort_by(|a, b| a.name.cmp(&b.name));
    std::fs::write(
        root.join("results.json"),
        serde_json::to_string_pretty(&results).unwrap_or_else(|_| "[]".into()),
    )
}

fn git_sha() -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(workspace_root())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn now_iso8601() -> String {
    // Minimal ISO-8601 without bringing in chrono. Seconds since epoch
    // is enough for diag bookkeeping; agents that need richer time
    // parse this with their host's clock as a sanity check.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

fn rust_ntfs_path() -> PathBuf {
    // Cargo provides this env var for [[bin]] entries when compiling
    // integration tests in the same package.
    PathBuf::from(env!("CARGO_BIN_EXE_rust-ntfs"))
}

fn f<S: Into<String>>(msg: S) -> Failed {
    Failed::from(msg.into())
}

// ---------------------------------------------------------------------------
// Mac-op dispatcher
// ---------------------------------------------------------------------------
//
// Parses a pure-mac operation_sequence and runs each op via the
// rust-ntfs CLI. The image is already allocated by the caller; this
// function calls the appropriate subcommand for each op in order.
//
// Op syntax accepted:
//   mac:format
//   mac:mkdir(/path)
//   mac:touch(/path)
//   mac:write(/path='content')              -- literal content
//   mac:write(/path bytes=N pattern=zeros)  -- generated bytes
//   mac:rm(/path)
//   mac:rmdir(/path)
//   mac:enumerate(...)                      -- arg is informational; we
//                                              just run `ls` and log.
//
// Each op's stdout/stderr is captured to diag/<op-N>-*.txt so a
// post-mortem agent can reconstruct what happened.

fn run_mac_ops(
    scn: &Scenario,
    img: &Path,
    diag: &Path,
    v: &mut Verbose,
) -> Result<(), ScenarioFailure> {
    let bin = rust_ntfs_path();
    let mut step = 0usize;
    for raw in scn.operation_sequence.split("->").map(str::trim) {
        if raw.is_empty() {
            continue;
        }
        step += 1;
        let desc = describe_op(raw, scn);
        v.step_start(&desc);
        match run_one_mac_op(raw, &bin, img, diag, step, scn) {
            Ok(()) => v.step_ok(),
            Err(e) => {
                let reason = scenario_failure_msg(&e);
                v.step_fail(&reason);
                return Err(ScenarioFailure::Errored(format!(
                    "step {step} ({raw}): {reason}"
                )));
            }
        }
    }
    Ok(())
}

fn run_one_mac_op(
    raw: &str,
    bin: &Path,
    img: &Path,
    diag: &Path,
    step: usize,
    scn: &Scenario,
) -> Result<(), ScenarioFailure> {
    let (verb, arg) = split_op(raw);
    match verb {
        "mac:format" => spawn(
            bin,
            &[
                "format",
                "-L",
                &scn.volume_params.label,
                "-c",
                &scn.volume_params.cluster_size.to_string(),
                "--serial",
                "deadbeefcafe1234",
                &img.display().to_string(),
            ],
            diag,
            step,
            "format",
        ),
        "mac:mkdir" => {
            let path = arg.ok_or_else(|| err("mac:mkdir requires a path argument"))?;
            let (parent, name) = split_parent_name(path)?;
            spawn(
                bin,
                &["mkdir", &img.display().to_string(), parent, name],
                diag,
                step,
                "mkdir",
            )
        }
        "mac:touch" => {
            let path = arg.ok_or_else(|| err("mac:touch requires a path argument"))?;
            let (parent, name) = split_parent_name(path)?;
            spawn(
                bin,
                &["touch", &img.display().to_string(), parent, name],
                diag,
                step,
                "touch",
            )
        }
        "mac:write" => {
            // Two arg shapes:
            //   /path='content'
            //   /path bytes=N [pattern=...]
            let arg = arg.ok_or_else(|| err("mac:write requires arguments"))?;
            if let Some((path, content)) = arg.split_once('=') {
                let content = content.trim().trim_matches(['"', '\'']);
                spawn(
                    bin,
                    &[
                        "write",
                        &img.display().to_string(),
                        path.trim(),
                        "--content",
                        content,
                    ],
                    diag,
                    step,
                    "write",
                )
            } else {
                Err(err(format!(
                    "mac:write expects path='content' form, got: {arg}"
                )))
            }
        }
        "mac:rm" => {
            let path = arg.ok_or_else(|| err("mac:rm requires a path argument"))?;
            spawn(
                bin,
                &["rm", &img.display().to_string(), path],
                diag,
                step,
                "rm",
            )
        }
        "mac:rmdir" => {
            let path = arg.ok_or_else(|| err("mac:rmdir requires a path argument"))?;
            spawn(
                bin,
                &["rmdir", &img.display().to_string(), path],
                diag,
                step,
                "rmdir",
            )
        }
        "mac:enumerate" => spawn(
            bin,
            &["ls", "-t", &img.display().to_string()],
            diag,
            step,
            "ls",
        ),
        "mac:set-dirty" => spawn(
            bin,
            &["set-dirty", &img.display().to_string()],
            diag,
            step,
            "set-dirty",
        ),
        other => Err(err(format!("unknown mac-op verb: {other}"))),
    }
}

fn split_op(raw: &str) -> (&str, Option<&str>) {
    if let Some(open) = raw.find('(') {
        if let Some(close) = raw.rfind(')') {
            let verb = raw[..open].trim();
            let arg = raw[open + 1..close].trim();
            return (verb, if arg.is_empty() { None } else { Some(arg) });
        }
    }
    (raw.trim(), None)
}

fn split_parent_name(path: &str) -> Result<(&str, &str), ScenarioFailure> {
    let path = path.trim_end_matches('/');
    let idx = path
        .rfind('/')
        .ok_or_else(|| err(format!("path {path} has no parent (must start with /)")))?;
    let parent = if idx == 0 { "/" } else { &path[..idx] };
    let name = &path[idx + 1..];
    if name.is_empty() {
        return Err(err(format!("path {path} has empty basename")));
    }
    Ok((parent, name))
}

fn spawn(
    bin: &Path,
    args: &[&str],
    diag: &Path,
    step: usize,
    label: &str,
) -> Result<(), ScenarioFailure> {
    let out = Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| err(format!("spawn rust-ntfs {label}: {e}")))?;
    let _ = std::fs::write(
        diag.join(format!("step{step:02}-{label}-stdout.txt")),
        &out.stdout,
    );
    let _ = std::fs::write(
        diag.join(format!("step{step:02}-{label}-stderr.txt")),
        &out.stderr,
    );
    if !out.status.success() {
        return Err(err(format!(
            "rust-ntfs {label} exit {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

fn err<S: Into<String>>(msg: S) -> ScenarioFailure {
    ScenarioFailure::Errored(msg.into())
}

// ---------------------------------------------------------------------------
// --verbose tree report
// ---------------------------------------------------------------------------

fn scenario_failure_msg(e: &ScenarioFailure) -> String {
    match e {
        ScenarioFailure::Errored(m) => m.clone(),
        ScenarioFailure::ChkdskFail { ro, scan } => {
            format!("chkdsk ro={ro} scan={scan}")
        }
    }
}

fn scenario_summary(scn: &Scenario) -> String {
    format!(
        "{}MiB / cluster={}B / label='{}'",
        scn.volume_params.size_mib,
        scn.volume_params.cluster_size,
        if scn.volume_params.label.is_empty() {
            "(empty)"
        } else {
            scn.volume_params.label.as_str()
        },
    )
}

// Human-readable description of an op, parameterised by scenario so the
// reader can see *what* is being tested in *which* scenario context
// (e.g. the volume size/cluster behind a `mac:format`, or the cycle
// count behind a `win:repeat-mount(N)`).
fn describe_op(raw: &str, scn: &Scenario) -> String {
    let (verb, arg) = split_op(raw);
    match verb {
        "mac:format" => format!(
            "format {}MiB volume, {}B clusters, label '{}' via rust-ntfs",
            scn.volume_params.size_mib, scn.volume_params.cluster_size, scn.volume_params.label,
        ),
        "mac:mkdir" => format!("create directory {}", arg.unwrap_or("?")),
        "mac:touch" => format!("create empty file {}", arg.unwrap_or("?")),
        "mac:write" => match arg.and_then(|a| a.split_once('=')) {
            Some((path, content)) => {
                let trimmed = content.trim().trim_matches(['"', '\'']);
                format!("write {:?} to {}", trimmed, path.trim())
            }
            None => format!("write {}", arg.unwrap_or("?")),
        },
        "mac:rm" => format!("remove file {}", arg.unwrap_or("?")),
        "mac:rmdir" => format!("remove empty directory {}", arg.unwrap_or("?")),
        "mac:enumerate" => "list volume contents via rust-ntfs ls".into(),
        "mac:set-dirty" => "mark volume dirty (test helper)".into(),
        "win:repeat-mount" => match arg {
            Some(n) => format!("{n}x dismount/remount cycle on Windows"),
            None => "dismount/remount cycle on Windows".into(),
        },
        "win:chkdsk" => {
            let v = match scn.verdict_shape {
                VerdictShape::Clean => "clean",
                VerdictShape::RepairOk => "repair-ok",
                VerdictShape::RepairRequired => "repair-required",
            };
            format!("chkdsk readonly + /scan via ntfs.sys (verdict: {v})")
        }
        "win:enumerate" => "enumerate volume tree under Windows".into(),
        "win:write" => match arg {
            Some(a) => format!("Windows write: {a}"),
            None => "Windows write".into(),
        },
        "win:delete" => match arg {
            Some(a) => format!("Windows delete: {a}"),
            None => "Windows delete".into(),
        },
        "win:dismount" => "dismount Windows volume".into(),
        "win:remount" => "remount Windows volume".into(),
        other => format!("{other} (no description)"),
    }
}
