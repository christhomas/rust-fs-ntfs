#!/usr/bin/env bash
# vm-clean-images.sh -- delete the disk images a matrix run leaves on the VM.
#
# Every scenario ships a .img to {vm.workdir} and the Windows ops wrap it in
# a .vhd of the same size, so a 16 GiB scenario occupies 32 GiB there. The
# ops delete both when they finish, which covers the normal path and is why
# this script is not a substitute for them.
#
# IT IS THE CRASHED PATH THAT NEEDS SWEEPING. On 2026-09-18 the VM stopped
# responding mid-run (System log 6008 / Kernel-Power 41); every op that was
# holding an image died with it, and the run left 51 files and 30.7 GiB
# behind -- enough that the NEXT run failed to ship a 4 GiB image at all
# (`Connection reset by peer`) because C: had no room. Nothing on the VM
# reclaims those, and nothing on the host knew to look.
#
# `chore matrix` runs this BEFORE the scenarios (once the VM is up, when the
# wreckage of the last run is visible) and again AFTER them. Sweeping at the
# start is the half that matters: a run that dies is exactly the run whose
# own cleanup never happened.
#
# SWEEPING EVERYTHING IS SAFE BECAUSE A VM RUNS ONE MATRIX AT A TIME. The
# images live flat in {vm.workdir} under a name derived from the scenario
# (nfs-<scenario>.img), with no run id in the path, so two concurrent runs
# would already be overwriting each other's images. The exclusivity is a
# property of the layout, not something this script introduces.
set -uo pipefail

# The one line this prints, from the four numbers the VM reports. Kept apart
# from the SSH so tests/scripts/vm-clean-images.sh can check the wording
# without a VM: `<count> <bytes> <freebytes>` in, one sentence out.
clean_says() {
    local count bytes free
    read -r count bytes free
    : "${count:=0}" "${bytes:=0}" "${free:=0}"
    if [ "$count" -eq 0 ]; then
        printf 'vm:clean: no images left behind — C: has %s GiB free\n' "$((free / 1073741824))"
    else
        printf 'vm:clean: removed %s image file(s), %s GiB — C: has %s GiB free\n' \
            "$count" "$((bytes / 1073741824))" "$((free / 1073741824))"
    fi
}

main() {
    REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
    cd "$REPO" || exit 1
    # shellcheck disable=SC1091
    [ -f .test-env ] && . ./.test-env
    : "${VM_HOST:?VM_HOST is not set in .test-env}" "${VM_WORKDIR:?VM_WORKDIR is not set in .test-env}"

    key_opts=()
    [ -n "${SSH_KEY:-}" ] && key_opts=(-o IdentitiesOnly=yes -i "$SSH_KEY")
    ssh_opts=(-o BatchMode=yes -o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new "${key_opts[@]+"${key_opts[@]}"}")

    # A VM that is down has no images to sweep and is not this script's
    # problem to report: `chore matrix` depends on vm:up, which says so far
    # better than a cleanup step can.
    if ! ssh "${ssh_opts[@]}" "$VM_HOST" exit 2>/dev/null; then
        echo "vm:clean: VM unreachable — nothing swept"
        return 0
    fi

    # PowerShell prints exactly three numbers; the shell decides the wording.
    local ps counts
    ps="\$w = '$VM_WORKDIR';
        \$f = @(Get-ChildItem \$w -Include *.img,*.vhd -Recurse -ErrorAction SilentlyContinue);
        \$b = (\$f | Measure-Object Length -Sum).Sum;
        if (\$null -eq \$b) { \$b = 0 };
        \$f | Remove-Item -Force -ErrorAction SilentlyContinue;
        \$free = (Get-CimInstance Win32_LogicalDisk -Filter \"DeviceID='C:'\").FreeSpace;
        '{0} {1} {2}' -f \$f.Count, \$b, \$free"

    counts="$(ssh "${ssh_opts[@]}" "$VM_HOST" \
        "powershell -NoProfile -NonInteractive -Command -" <<<"$ps" 2>/dev/null | tr -d '\r')"

    if [ -z "$counts" ]; then
        echo "vm:clean: the VM reported nothing — images may still be there"
        return 0
    fi
    printf '%s\n' "$counts" | clean_says
}

# Sourced by its test, run by chore.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    main "$@"
fi
