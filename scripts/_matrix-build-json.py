#!/usr/bin/env python3
"""Build test-diagnostics/matrix-results.json from harness stdout + VM artifacts.

Invoked by `scripts/_matrix-collect-vm.sh`. Not intended to be called
directly. Consumes:

  --matrix-log   stdout/stderr of a `bash scripts/run-matrix.sh` run
  --vm-info      JSON from scripts/win/vm-info.ps1
  --verdicts     JSON from scripts/win/verdict-collect.ps1
  --output       path to write matrix-results.json

Produces a structured JSON keyed for diff-stability (sorted scenarios,
explicit schema_version field).
"""
import argparse
import json
import re
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path


def run(cmd):
    return subprocess.check_output(cmd, shell=True, text=True).strip()


def parse_scenarios(log_path):
    """Parse harness stdout for `test NAME ... ok|FAILED` lines."""
    scenarios = {}
    pat = re.compile(r"^test ([a-z0-9-]+)\s+\.\.\.\s+(ok|FAILED)$")
    for line in Path(log_path).read_text().splitlines():
        m = pat.match(line)
        if m:
            scenarios[m.group(1)] = {"status": m.group(2)}
    return scenarios


def parse_total_duration(log_path):
    pat = re.compile(r"^test result: .*finished in (\d+\.\d+)s$")
    for line in Path(log_path).read_text().splitlines():
        m = pat.match(line)
        if m:
            return float(m.group(1))
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--matrix-log", required=True)
    ap.add_argument("--vm-info", required=True)
    ap.add_argument("--verdicts", required=True)
    ap.add_argument("--output", required=True)
    args = ap.parse_args()

    repo = Path(run("git rev-parse --show-toplevel"))
    head_sha = run("git rev-parse HEAD")
    branch = run("git rev-parse --abbrev-ref HEAD")
    binary_path = repo / "target" / "release" / "rust-ntfs"
    if not binary_path.exists():
        sys.exit(f"binary not built: {binary_path}")
    binary_sha = run(f"sha256sum {binary_path} | awk '{{print $1}}'")
    matrix_json_sha = run(f"sha256sum {repo}/test-matrix.json | awk '{{print $1}}'")
    harness_sha = run(f"git -C {repo}/vendor/fs-test-harness rev-parse HEAD")

    vm = json.loads(Path(args.vm_info).read_text())
    verdicts = json.loads(Path(args.verdicts).read_text())
    scenarios = parse_scenarios(args.matrix_log)

    # Merge verdict data into scenarios
    for name, v in verdicts.items():
        if name not in scenarios:
            # Verdict exists but harness didn't report a test line for it —
            # likely a scenario that was skipped or filtered out.
            scenarios[name] = {"status": "unknown"}
        scenarios[name]["verdict_shape"] = v.get("verdict_shape")
        scenarios[name]["exits"] = v.get("exits", {})

    out = {
        "schema_version": 1,
        "tested_at_sha": head_sha,
        "tested_at_branch": branch,
        "tested_at_utc": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "binary_sha256": binary_sha,
        "test_matrix_json_sha256": matrix_json_sha,
        "harness_submodule_sha": harness_sha,
        "host": {
            "os": run("uname -s"),
            "version": run("uname -r"),
            "arch": run("uname -m"),
        },
        "vm": {
            "address": run("source .test-env && echo $VM_HOST"),
            "os_caption": vm.get("os_caption"),
            "os_build": vm.get("os_build"),
            "os_version": vm.get("os_version"),
            "os_arch": vm.get("os_arch"),
            "ntfs_sys": vm.get("ntfs_sys"),
            "chkdsk_version": vm.get("chkdsk_version"),
            "powershell_version": vm.get("powershell_version"),
        },
        "scenarios": dict(sorted(scenarios.items())),
        "summary": {
            "passed": sum(1 for s in scenarios.values() if s["status"] == "ok"),
            "failed": sum(1 for s in scenarios.values() if s["status"] == "FAILED"),
            "unknown": sum(1 for s in scenarios.values() if s["status"] == "unknown"),
            "total": len(scenarios),
            "total_duration_s": parse_total_duration(args.matrix_log),
        },
    }

    Path(args.output).parent.mkdir(parents=True, exist_ok=True)
    Path(args.output).write_text(json.dumps(out, indent=2) + "\n")
    print(f"wrote {args.output}: {out['summary']}")


if __name__ == "__main__":
    main()
