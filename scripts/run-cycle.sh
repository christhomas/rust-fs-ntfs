#!/usr/bin/env bash
# run-cycle.sh -- iterate through pending scenarios, dispatch what we can.
#
# For each scenario we claim:
#   - If operation_sequence contains anything we don't support yet
#     (mac:write, mac:delete, mac:enumerate, win:format, win:write*,
#     win:delete), mark blocked-on-<missing-capability>.
#   - Otherwise (pure mac:format -> win:chkdsk), run the local pipeline
#     and mark passed/failed based on chkdsk readonly exit.
#
# Usage:
#   bash scripts/run-cycle.sh <session-name> [<cycle-tag>]
set -euo pipefail
SESSION="${1:?session name required}"
CYCLE_TAG="${2:-}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PARENT_REPO="/Volumes/sdcard256gb/projects/diskjockey/vendor/rust-fs-ntfs"
WORK_LIST_PARENT="${PARENT_REPO}/test-matrix.json"
DIAG_BASE="${TMPDIR:-/tmp}/rust-fs-ntfs-diag/${SESSION}"
mkdir -p "${DIAG_BASE}"

# Fail fast if the harness submodule isn't initialised, rather than
# silently looping forever with claim-scenario.sh missing.
CLAIM_SCRIPT="${PARENT_REPO}/harness/scripts/claim-scenario.sh"
UPDATE_SCRIPT="${PARENT_REPO}/harness/scripts/update-scenario-status.sh"
[[ -x "${CLAIM_SCRIPT}" ]] || { echo "missing executable: ${CLAIM_SCRIPT} (run: git submodule update --init --recursive)" >&2; exit 1; }
[[ -x "${UPDATE_SCRIPT}" ]] || { echo "missing executable: ${UPDATE_SCRIPT} (run: git submodule update --init --recursive)" >&2; exit 1; }

SUFFIX="${SESSION}"
if [[ -n "${CYCLE_TAG}" ]]; then
    SUFFIX="${CYCLE_TAG}-${SESSION}"
fi

while true; do
    # Atomic claim against the parent submodule's shared work list.
    # `|| true` — claim-scenario.sh exits 1 when no pending scenarios
    # remain (normal end-of-loop signal), and we want stderr to come
    # through for genuine errors so they're visible.
    NEXT="$(cd "${PARENT_REPO}" && bash harness/scripts/claim-scenario.sh "${SESSION}" || true)"
    if [[ -z "${NEXT}" ]]; then
        echo "no more pending scenarios (or claim race exhausted)"
        break
    fi
    echo "=== claimed: ${NEXT} ==="

    # Read the scenario's params from the parent's work list.
    PARAMS_JSON="$(python3 -c "
import json
with open('${WORK_LIST_PARENT}') as f:
    d = json.load(f)
e = d['scenarios']['${NEXT}']
import sys
print(json.dumps({
    'op_seq': e.get('operation_sequence',''),
    'size_mib': e.get('volume_params',{}).get('size_mib', 256),
    # alloc_unit_size is the harness-vocabulary name; cluster_size is
    # accepted as a legacy fallback during the v1->v2 migration.
    'cluster_size': e.get('volume_params',{}).get('alloc_unit_size',
                          e.get('volume_params',{}).get('cluster_size', 4096)),
    'label': e.get('volume_params',{}).get('label', 'CITEST'),
}))
")"
    OP_SEQ="$(echo "${PARAMS_JSON}" | python3 -c 'import json,sys;print(json.load(sys.stdin)["op_seq"])')"
    SIZE_MIB="$(echo "${PARAMS_JSON}" | python3 -c 'import json,sys;print(json.load(sys.stdin)["size_mib"])')"
    CLUSTER="$(echo "${PARAMS_JSON}" | python3 -c 'import json,sys;print(json.load(sys.stdin)["cluster_size"])')"
    LABEL="$(echo "${PARAMS_JSON}" | python3 -c 'import json,sys;print(json.load(sys.stdin)["label"])')"

    echo "  op_seq: ${OP_SEQ}"
    echo "  size=${SIZE_MIB} cluster=${CLUSTER} label=${LABEL}"

    # Triage: any unsupported op classes?
    BLOCKED_REASON=""
    case "${OP_SEQ}" in
        *mac:write*) BLOCKED_REASON="needs-mac-writer" ;;
        *mac:delete*) BLOCKED_REASON="needs-mac-deleter" ;;
        *mac:enumerate*)
            # mac:enumerate alone is in scope per protocol (the inspect CLI),
            # but the scenarios that need it also chain mac:write upstream
            # — those got caught by *mac:write* above. A pure
            # mac:format -> mac:enumerate needs the inspect CLI; mark.
            BLOCKED_REASON="needs-mac-inspect-cli" ;;
        *win:format*) BLOCKED_REASON="needs-win-format-runner" ;;
        *win:write*) BLOCKED_REASON="needs-win-fixture-runner" ;;
        *win:delete*) BLOCKED_REASON="needs-win-fixture-runner" ;;
    esac

    if [[ -n "${BLOCKED_REASON}" ]]; then
        echo "  -> blocked-${BLOCKED_REASON}"
        # Best-effort: status updates are advisory; don't crash the
        # cycle on a transient race. Same policy as the completed-
        # status update at the bottom of the loop.
        (cd "${PARENT_REPO}" && bash harness/scripts/update-scenario-status.sh "${NEXT}" "blocked-${BLOCKED_REASON}-${SUFFIX}") || true
        continue
    fi

    # Compute wrapper size: volume + 128 MiB GPT slack (or 1.5x for very small).
    WRAPPER=$(( SIZE_MIB + 128 ))
    if (( SIZE_MIB < 128 )); then
        WRAPPER=$(( SIZE_MIB * 3 / 2 + 64 ))
    fi

    LOG="${DIAG_BASE}/run-${NEXT}.log"
    echo "  starting run, log: ${LOG}"
    set +e
    VOLUME_SIZE_MB="${SIZE_MIB}" \
        WRAPPER_SIZE_MB="${WRAPPER}" \
        LABEL="${LABEL}" \
        CLUSTER_SIZE="${CLUSTER}" \
        VM_WORKDIR="C:/Users/chris/dev/rust-fs-ntfs-${SESSION}" \
        DIAG_DIR="${DIAG_BASE}" \
        bash "${REPO_ROOT}/scripts/test-windows-local.sh" > "${LOG}" 2>&1
    RC=$?
    set -e

    # Extract verdict. PowerShell tee writes the chkdsk exit lines in
    # UTF-16-LE, so plain grep misses them; pipe through `strings` first.
    VERDICT_LINE="$(strings "${LOG}" 2>/dev/null | grep -E 'chkdsk verdict' | tail -1 || true)"
    READONLY_EXIT="$(echo "${VERDICT_LINE}" | grep -oE 'readonly=[0-9]+' | head -1 | cut -d= -f2 || echo '?')"
    SCAN_EXIT="$(echo "${VERDICT_LINE}" | grep -oE 'scan=[0-9]+' | head -1 | cut -d= -f2 || echo '?')"
    DIAG_PATH="$(grep '^diag dir:' "${LOG}" | tail -1 | awk '{print $3}' || echo "${DIAG_BASE}")"
    # Empty values default to '?'.
    : "${READONLY_EXIT:=?}"
    : "${SCAN_EXIT:=?}"

    if [[ "${READONLY_EXIT}" == "0" ]]; then
        STATUS="passed-${SUFFIX}"
    elif [[ "${RC}" -ne 0 ]] && [[ "${READONLY_EXIT}" == "?" ]]; then
        STATUS="failed-pipeline-error-${SUFFIX}"
    elif grep -q 'frs.cxx' "${LOG}" 2>/dev/null || grep -q '6672732e637878' "${LOG}" 2>/dev/null; then
        STATUS="failed-frs-cxx-60f-tail-${SUFFIX}"
    elif grep -q 'Cannot open volume' "${LOG}" 2>/dev/null; then
        STATUS="failed-mount-collision-${SUFFIX}"
    else
        STATUS="failed-readonly=${READONLY_EXIT}-scan=${SCAN_EXIT}-${SUFFIX}"
    fi

    echo "  -> ${STATUS} (readonly=${READONLY_EXIT} scan=${SCAN_EXIT} rc=${RC})"
    (cd "${PARENT_REPO}" && bash harness/scripts/update-scenario-status.sh "${NEXT}" "${STATUS}" "${DIAG_PATH}") || true
done

echo "cycle done"
