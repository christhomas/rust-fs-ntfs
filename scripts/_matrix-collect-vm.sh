#!/usr/bin/env bash
# scripts/_matrix-collect-vm.sh — internal helper used by
# matrix-baseline.sh. Collects VM metadata + per-scenario verdict.json
# files from the Windows test VM, parses the harness stdout, and writes
# test-diagnostics/matrix-results.json.
#
# Usage:
#   bash scripts/_matrix-collect-vm.sh <matrix-stdout-log>
#
# Requires Python 3 (uses it as a structured-JSON builder).

set -euo pipefail

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <matrix-stdout-log>" >&2
    exit 2
fi

matrix_log="$1"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

# shellcheck disable=SC1091
source .test-env

# Ship the small VM info collector if not already there
ssh -i "$SSH_KEY" -o StrictHostKeyChecking=no "$VM_HOST" \
    "powershell -Command \"New-Item -Force -ItemType Directory -Path '$VM_WORKDIR' | Out-Null\"" >/dev/null

scp -i "$SSH_KEY" -o StrictHostKeyChecking=no \
    "$repo_root/scripts/win/vm-info.ps1" "$VM_HOST:$VM_WORKDIR/vm-info.ps1" >/dev/null
scp -i "$SSH_KEY" -o StrictHostKeyChecking=no \
    "$repo_root/scripts/win/verdict-collect.ps1" "$VM_HOST:$VM_WORKDIR/verdict-collect.ps1" >/dev/null

# Gather VM info
vm_info_json="$(mktemp -t vm-info.XXXXXX.json)"
ssh -i "$SSH_KEY" -o StrictHostKeyChecking=no "$VM_HOST" \
    "powershell -ExecutionPolicy Bypass -File $VM_WORKDIR/vm-info.ps1" \
    > "$vm_info_json"

# Gather per-scenario verdicts
verdicts_json="$(mktemp -t verdicts.XXXXXX.json)"
ssh -i "$SSH_KEY" -o StrictHostKeyChecking=no "$VM_HOST" \
    "powershell -ExecutionPolicy Bypass -File $VM_WORKDIR/verdict-collect.ps1" \
    > "$verdicts_json"

trap 'rm -f "$vm_info_json" "$verdicts_json"' EXIT

# Hand off to the Python builder
python3 "$repo_root/scripts/_matrix-build-json.py" \
    --matrix-log "$matrix_log" \
    --vm-info "$vm_info_json" \
    --verdicts "$verdicts_json" \
    --output "$repo_root/test-diagnostics/matrix-results.json"
