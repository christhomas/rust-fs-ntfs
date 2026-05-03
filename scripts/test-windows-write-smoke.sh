#!/usr/bin/env bash
# test-windows-write-smoke.sh -- minimal write-one-file diagnostic on top of mkfs_ntfs.
#
# Skips the chkdsk matrix entirely. The matrix tests assert chkdsk
# verdicts on the volume layout; this script asserts the only thing
# that actually matters to a user: can ntfs.sys write a file to a
# freshly-formatted volume?
#
# Reuses the same VM-scaffolding pattern as test-windows-matrix.sh
# (tar source over SSH, build mkfs_ntfs on the VM, drive a PowerShell
# script, tar diag back).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ -f "${REPO_ROOT}/.test-env" ]]; then
    # shellcheck disable=SC1091
    source "${REPO_ROOT}/.test-env"
fi

if [[ -z "${VM_HOST:-}" ]]; then
    echo "VM_HOST is not set. Run scripts/setup-local.sh first, or export VM_HOST=user@host." >&2
    exit 2
fi
VM_WORKDIR="${VM_WORKDIR:-C:/Users/${VM_HOST%%@*}/dev/rust-fs-ntfs-matrix}"
SSH_OPTS="${SSH_OPTS:-}"
# Always-on SSH timeouts — see test-windows-matrix.sh for rationale.
SSH_OPTS="${SSH_OPTS} -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=4"
VM_WORKDIR_PS="${VM_WORKDIR//\//\\}"

VOLUME_MIB="${VOLUME_MIB:-256}"
CLUSTER_SIZE="${CLUSTER_SIZE:-4096}"
LABEL="${LABEL:-SMOKE}"

TIMESTAMP="$(date +%Y%m%d-%H%M%S)"
DIAG_BASE="${DIAG_DIR:-${REPO_ROOT}/test-diagnostics}"
DIAG_LOCAL="${DIAG_BASE}/write-smoke-${TIMESTAMP}"
mkdir -p "${DIAG_LOCAL}"

cd "${REPO_ROOT}"

echo "[push] tar-ssh source -> ${VM_HOST}:${VM_WORKDIR}"
ssh ${SSH_OPTS} "${VM_HOST}" "if (-not (Test-Path '${VM_WORKDIR_PS}')) { New-Item -ItemType Directory -Path '${VM_WORKDIR_PS}' -Force | Out-Null }"
tar --exclude='./target' --exclude='./.git' --exclude='./diag' \
    --exclude='./diag-local-*' --exclude='./test-disks' \
    --exclude='./test-diagnostics' \
    --exclude='./tarpaulin-report.html' --exclude='*.swp' \
    --exclude='.DS_Store' --exclude='./privatekey' --exclude='./privatekey.*' \
    -cf - . | \
    ssh ${SSH_OPTS} "${VM_HOST}" "tar -xf - -C '${VM_WORKDIR}'"

# Build mkfs_ntfs.exe, format a fresh image with our mkfs, then drive
# the write-smoke PowerShell. Image / VHDX / diag paths are stable
# across runs (overwritten each invocation) — diag is tar-pulled back
# at the end so timestamping happens on the Mac side.
REMOTE_CMD="\$ErrorActionPreference='Stop';
Set-Location '${VM_WORKDIR_PS}';
\$env:RUSTUP_TOOLCHAIN='stable-aarch64-pc-windows-gnullvm';
\$env:PATH=\"\$env:USERPROFILE\\.cargo\\bin;C:\\Program Files\\Cloudbase Solutions\\QEMU\\bin;\$env:PATH\";
cargo build --release --bin mkfs_ntfs;
if (\$LASTEXITCODE -ne 0) { exit \$LASTEXITCODE };
\$img = '${VM_WORKDIR_PS}\\nfs-write-smoke.img';
\$vhdx = '${VM_WORKDIR_PS}\\wrapper-write-smoke.vhdx';
\$diag = '${VM_WORKDIR_PS}\\diag\\write-smoke';
if (Test-Path \$img) { Remove-Item \$img -Force };
if (Test-Path \$vhdx) { Remove-Item \$vhdx -Force };
if (Test-Path \$diag) { Remove-Item \$diag -Recurse -Force };
New-Item -ItemType Directory -Path \$diag -Force | Out-Null;
\$f = [System.IO.File]::Create(\$img);
\$f.SetLength(${VOLUME_MIB} * 1MB);
\$f.Close();
\$mkfs = '${VM_WORKDIR_PS}\\target\\release\\mkfs_ntfs.exe';
\$mkfsArgs = @('-L', '${LABEL}', '-c', '${CLUSTER_SIZE}', '--serial', 'deadbeefcafe1234', \$img);
\$mkfsProc = Start-Process -FilePath \$mkfs -ArgumentList \$mkfsArgs -NoNewWindow -PassThru -Wait -RedirectStandardOutput \"\$diag\\mkfs-stdout.txt\" -RedirectStandardError \"\$diag\\mkfs-stderr.txt\";
if (\$mkfsProc.ExitCode -ne 0) { Write-Host \"mkfs failed exit=\$(\$mkfsProc.ExitCode)\"; exit \$mkfsProc.ExitCode };
& powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File '${VM_WORKDIR_PS}\\scripts\\run-write-smoke.ps1' -Img \$img -Vhdx \$vhdx -Diag \$diag -VolumeSizeMb ${VOLUME_MIB};
exit \$LASTEXITCODE"

echo "[run]  mkfs_ntfs (${VOLUME_MIB} MiB / ${CLUSTER_SIZE} cluster / label '${LABEL}') + write-smoke on ${VM_HOST}"
echo
set +e
ssh ${SSH_OPTS} "${VM_HOST}" "${REMOTE_CMD}"
RUN_EXIT=$?
set -e

echo
echo "[pull] diag -> ${DIAG_LOCAL}"
ssh ${SSH_OPTS} "${VM_HOST}" "Set-Location '${VM_WORKDIR_PS}'; if (Test-Path 'diag\\write-smoke') { tar -cf - diag/write-smoke } else { exit 0 }" | \
    tar -xf - -C "${DIAG_LOCAL}" --strip-components=2 2>/dev/null || \
    echo "[pull] no diag/write-smoke on remote"

echo
echo "═══════════════════════════════════════════════════════════════"
if [[ -f "${DIAG_LOCAL}/write-exit.txt" ]]; then
    WRITE_OK="$(tr -d '[:space:]' < "${DIAG_LOCAL}/write-exit.txt")"
    if [[ "${WRITE_OK}" == "1" ]]; then
        echo "verdict: WRITE SUCCEEDED (file landed on volume)"
    else
        echo "verdict: WRITE FAILED — see write-attempt.txt for the exception"
    fi
fi
echo "diagnostics: ${DIAG_LOCAL}"
echo "ssh exit:    ${RUN_EXIT}"
echo "═══════════════════════════════════════════════════════════════"

exit ${RUN_EXIT}
