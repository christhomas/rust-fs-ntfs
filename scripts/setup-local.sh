#!/usr/bin/env bash
# setup-local.sh -- one-shot human-facing local-scaffold setup.
#
# Purpose: walk a new contributor from "I have a Windows VM running"
# to "I can run the test matrix" without them needing to memorise
# env vars or SSH flags.
#
# What it does:
#   1. Prompt for VM SSH connection details (host, user, key path).
#   2. Verify SSH actually connects + the VM looks like Windows.
#   3. Run setup-windows-vm.sh once to install rustup / LLVM-MinGW /
#      qemu-img on the VM (idempotent — safe to re-run).
#   4. Save the resolved settings to .test-env (gitignored) so
#      test-windows-matrix.sh picks them up automatically.
#
# After this, contributors just run:
#   bash scripts/test-windows-matrix.sh
# with no env vars, no SSH flags.
#
# In CI nothing here runs — the testing core (cargo test --test matrix)
# executes directly on the windows-latest runner. This script is only
# the local scaffold's onboarding step.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="${REPO_ROOT}/.test-env"

# ─── Load existing config if present ───────────────────────────────
PRIOR_VM_HOST=""
PRIOR_SSH_KEY=""
PRIOR_VM_WORKDIR=""
if [[ -f "${ENV_FILE}" ]]; then
    # shellcheck disable=SC1090
    source "${ENV_FILE}"
    PRIOR_VM_HOST="${VM_HOST:-}"
    PRIOR_VM_WORKDIR="${VM_WORKDIR:-}"
    # SSH_KEY isn't an env var of its own — extract from SSH_OPTS if present.
    if [[ "${SSH_OPTS:-}" == *"-i "* ]]; then
        PRIOR_SSH_KEY="$(echo "${SSH_OPTS}" | sed -n 's/.*-i \([^ ]*\).*/\1/p')"
    fi
fi

prompt() {
    # prompt VAR_NAME "Question" "default"
    local var="$1" question="$2" default="${3:-}"
    local input
    if [[ -n "${default}" ]]; then
        read -r -p "${question} [${default}]: " input
        printf -v "${var}" '%s' "${input:-${default}}"
    else
        read -r -p "${question}: " input
        printf -v "${var}" '%s' "${input}"
    fi
}

echo "═══════════════════════════════════════════════════════════════"
echo " rust-fs-ntfs — local test scaffold setup"
echo "═══════════════════════════════════════════════════════════════"
echo
echo "This sets up your Mac to dispatch the NTFS test matrix to a"
echo "Windows VM you've already started. You'll need:"
echo "  • The VM's IP (or hostname)"
echo "  • A user account on the VM with admin rights (chkdsk + Mount-DiskImage need it)"
echo "  • An SSH private key that account accepts (no password prompt)"
echo

prompt VM_USER "VM username" "${PRIOR_VM_HOST%%@*}"
prompt VM_IP   "VM IP / hostname" "${PRIOR_VM_HOST##*@}"
VM_HOST="${VM_USER}@${VM_IP}"

# Default key: existing repo-bundled privatekey if present, else prior, else
# leave empty (let ssh-agent handle it).
DEFAULT_KEY=""
if [[ -n "${PRIOR_SSH_KEY}" ]]; then
    DEFAULT_KEY="${PRIOR_SSH_KEY}"
elif [[ -f "${REPO_ROOT}/privatekey" ]]; then
    DEFAULT_KEY="${REPO_ROOT}/privatekey"
fi
prompt SSH_KEY "SSH private key (blank = use ssh-agent)" "${DEFAULT_KEY}"

prompt VM_WORKDIR "Remote workdir on the VM" "${PRIOR_VM_WORKDIR:-C:/Users/${VM_USER}/dev/rust-fs-ntfs-matrix}"

SSH_OPTS=""
if [[ -n "${SSH_KEY}" ]]; then
    if [[ ! -f "${SSH_KEY}" ]]; then
        echo "  ✗ SSH key not found at: ${SSH_KEY}"
        exit 1
    fi
    # IdentitiesOnly stops ssh-agent from offering other keys first.
    SSH_OPTS="-i ${SSH_KEY} -o IdentitiesOnly=yes"
fi

echo
echo "[verify] testing SSH connectivity..."
SSH_PROBE_OUT=$(ssh ${SSH_OPTS} -o ConnectTimeout=10 -o BatchMode=yes \
    "${VM_HOST}" 'powershell -NoProfile -Command "$PSVersionTable.OS; whoami"' 2>&1) || {
    echo "  ✗ SSH connection failed:"
    echo "${SSH_PROBE_OUT}" | sed 's/^/    /'
    echo
    echo "  Common fixes:"
    echo "    • Confirm the VM is running and reachable: ping ${VM_IP}"
    echo "    • Confirm the OpenSSH server is enabled on the VM"
    echo "    • Confirm your public key is in C:\\Users\\${VM_USER}\\.ssh\\authorized_keys"
    echo "    • Try connecting manually: ssh ${SSH_OPTS} ${VM_HOST}"
    exit 1
}
echo "  ✓ connected:"
echo "${SSH_PROBE_OUT}" | sed 's/^/    /'

case "${SSH_PROBE_OUT}" in
    *Windows*) ;;
    *)
        echo
        echo "  ⚠ that doesn't look like a Windows VM (no 'Windows' in OS string)."
        prompt CONTINUE_ANYWAY "Continue anyway? (y/N)" "n"
        [[ "${CONTINUE_ANYWAY}" =~ ^[Yy] ]] || exit 1
        ;;
esac

# ─── Provision the VM (rustup / LLVM-MinGW / qemu-img) ─────────────
echo
echo "[verify] checking required tools on VM..."
TOOL_CHECK=$(ssh ${SSH_OPTS} "${VM_HOST}" 'powershell -NoProfile -Command "
    $missing = @()
    if (-not (Get-Command rustc -EA SilentlyContinue)) { $missing += \"rustc\" }
    if (-not (Get-Command qemu-img -EA SilentlyContinue)) {
        if (-not (Test-Path \"C:\Program Files\Cloudbase Solutions\QEMU\bin\qemu-img.exe\")) {
            $missing += \"qemu-img\"
        }
    }
    if (-not (Get-Command aarch64-w64-mingw32-clang -EA SilentlyContinue) -and
        -not (Get-Command x86_64-w64-mingw32-clang -EA SilentlyContinue)) {
        $missing += \"llvm-mingw\"
    }
    if ($missing.Count -eq 0) { Write-Output \"OK\" } else { Write-Output (\"MISSING:\" + ($missing -join \",\")) }
"' 2>&1)

if [[ "${TOOL_CHECK}" == "OK" ]]; then
    echo "  ✓ rustc + qemu-img + llvm-mingw all present"
else
    echo "  ⚠ missing: ${TOOL_CHECK#MISSING:}"
    echo
    echo "[provision] running setup-windows-vm.sh (this can take ~3 minutes)..."
    VM_HOST="${VM_HOST}" VM_WORKDIR="${VM_WORKDIR}" SSH_OPTS="${SSH_OPTS}" \
        bash "${REPO_ROOT}/scripts/setup-windows-vm.sh"
fi

# ─── Persist config ────────────────────────────────────────────────
cat > "${ENV_FILE}" <<EOF
# Generated by scripts/setup-local.sh on $(date '+%Y-%m-%d %H:%M:%S').
# Sourced automatically by scripts/test-windows-matrix.sh.
# Safe to re-run setup-local.sh to regenerate.
export VM_HOST="${VM_HOST}"
export VM_WORKDIR="${VM_WORKDIR}"
export SSH_OPTS="${SSH_OPTS}"
EOF

# .gitignore guard — don't commit a developer's VM details.
GITIGNORE="${REPO_ROOT}/.gitignore"
if [[ -f "${GITIGNORE}" ]] && ! grep -qxF '.test-env' "${GITIGNORE}"; then
    echo '.test-env' >> "${GITIGNORE}"
    echo "  ✓ added .test-env to .gitignore"
fi

echo
echo "═══════════════════════════════════════════════════════════════"
echo " ✓ Setup complete. Saved to ${ENV_FILE}"
echo
echo " Run the test matrix:"
echo "   bash scripts/test-windows-matrix.sh"
echo
echo " Run a single scenario:"
echo "   bash scripts/test-windows-matrix.sh mac-format-tiny-32mib"
echo
echo " List scenarios without running them:"
echo "   bash scripts/test-windows-matrix.sh --list"
echo "═══════════════════════════════════════════════════════════════"
