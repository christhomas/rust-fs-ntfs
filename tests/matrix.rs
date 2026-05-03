// matrix.rs — data-driven NTFS scenario runner.
//
// Each scenario in test-matrix.json becomes one libtest-mimic
// trial. The trial body delegates the Windows-side lifecycle to
// scripts/run-scenario.ps1 (which produces the byte-diff evidence
// packet); this file owns:
//   1. scenario filtering (phase-1 = mac:format → win:chkdsk*)
//   2. mkfs_ntfs invocation
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
use std::sync::Mutex;

// VHDX mount uses Windows-global state (drive letters, disk numbers).
// Serialise mount/chkdsk/dismount across trials. mkfs runs in parallel
// because each scenario gets a distinct .img path.
static MOUNT_LOCK: Mutex<()> = Mutex::new(());

#[derive(Deserialize)]
struct WorkList {
    scenarios: std::collections::BTreeMap<String, Scenario>,
}

#[derive(Deserialize, Clone)]
struct Scenario {
    volume_params: VolumeParams,
    operation_sequence: String,
    // Other fields (status, evidence_link, _attempts, ...) are
    // intentionally ignored — they're agent bookkeeping, not test input.
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
    let args = Arguments::from_args();

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
            let runnable_op = is_phase1_runnable(&scn.operation_sequence);
            if runnable_op {
                runnable += 1;
            }
            let body_name = name.clone();
            let trial = Trial::test(name, move || run_scenario(&body_name, &scn));
            if !runnable_op || !cfg!(target_os = "windows") {
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

// Phase 1 covers `mac:format -> win:chkdsk*`. Anything else (writes,
// deletes, mac-side enumerate) needs scaffolding that doesn't ship
// yet — those scenarios are reported as ignored so the suite makes
// the gap visible without failing.
fn is_phase1_runnable(sequence: &str) -> bool {
    let s = sequence.trim();
    if !s.starts_with("mac:format") {
        return false;
    }
    let blockers = [":write", ":delete", ":modify", "mac:enumerate"];
    !blockers.iter().any(|b| s.contains(b))
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn matrix_diag_root() -> PathBuf {
    workspace_root().join("diag/matrix")
}

fn run_scenario(name: &str, scn: &Scenario) -> Result<(), Failed> {
    if !cfg!(target_os = "windows") {
        return Err(Failed::from("non-Windows host — should have been ignored"));
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

    let outcome = run_scenario_inner(name, scn, &workdir, &diag, &img, &vhdx, &reference_vhdx);
    let elapsed = started.elapsed().as_secs_f64();

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

fn run_scenario_inner(
    _name: &str,
    scn: &Scenario,
    workdir: &Path,
    diag: &Path,
    img: &Path,
    vhdx: &Path,
    reference_vhdx: &Path,
) -> Result<(i32, i32), ScenarioFailure> {
    // 1. Allocate blank raw image.
    let f_img = std::fs::File::create(img)
        .map_err(|e| ScenarioFailure::Errored(format!("create img: {e}")))?;
    f_img
        .set_len(scn.volume_params.size_mib * 1024 * 1024)
        .map_err(|e| ScenarioFailure::Errored(format!("set_len: {e}")))?;
    drop(f_img);

    // 2. Format with our mkfs_ntfs.
    let mkfs = mkfs_path();
    let out = Command::new(&mkfs)
        .args([
            "-L",
            &scn.volume_params.label,
            "-c",
            &scn.volume_params.cluster_size.to_string(),
            "--serial",
            "deadbeefcafe1234",
        ])
        .arg(img)
        .output()
        .map_err(|e| ScenarioFailure::Errored(format!("spawn mkfs_ntfs: {e}")))?;
    let _ = std::fs::write(diag.join("mkfs-stdout.txt"), &out.stdout);
    let _ = std::fs::write(diag.join("mkfs-stderr.txt"), &out.stderr);
    if !out.status.success() {
        return Err(ScenarioFailure::Errored(format!(
            "mkfs_ntfs failed (exit {:?}): {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
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
        .output()
        .map_err(|e| ScenarioFailure::Errored(format!("spawn run-scenario.ps1: {e}")))?;

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let _ = std::fs::write(diag.join("ps-stdout.txt"), &stdout);
    let _ = std::fs::write(diag.join("ps-stderr.txt"), &stderr);

    if !out.status.success() {
        return Err(ScenarioFailure::Errored(format!(
            "run-scenario.ps1 failed (exit {:?}): {}",
            out.status.code(),
            stderr.trim()
        )));
    }

    let ro = parse_marker(&stdout, "RO_EXIT=").ok_or_else(|| {
        ScenarioFailure::Errored(format!("RO_EXIT not found in PS output:\n{stdout}"))
    })?;
    let scan = parse_marker(&stdout, "SCAN_EXIT=").ok_or_else(|| {
        ScenarioFailure::Errored(format!("SCAN_EXIT not found in PS output:\n{stdout}"))
    })?;

    if ro == 0 && scan == 0 {
        Ok((ro, scan))
    } else {
        Err(ScenarioFailure::ChkdskFail { ro, scan })
    }
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

fn mkfs_path() -> PathBuf {
    // Cargo provides this env var for [[bin]] entries when compiling
    // integration tests in the same package.
    PathBuf::from(env!("CARGO_BIN_EXE_mkfs_ntfs"))
}

fn f<S: Into<String>>(msg: S) -> Failed {
    Failed::from(msg.into())
}
