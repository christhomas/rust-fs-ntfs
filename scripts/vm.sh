#!/usr/bin/env bash
# vm.sh status|up|down|address|network -- drive the Windows test VM (VMware Fusion).
#
# Called by the chore vm:* tasks, which own WHEN each runs (`deps: [vm:up]`);
# this file owns HOW. It is a script rather than inline chore steps because
# chore prints a failing step's whole text, and a sixty-line step turns one
# clear error into a screenful.
#
# Settings come from .test-env (loaded by chore's dotenv, or by this script
# when run directly):
#   VM_VMX     the VM's .vmx path
#   VM_HOST    user@address. WRITTEN by `vm.sh up` from what it discovers
#              (see cmd_address), so every consumer of .test-env reads the
#              VM's live address; only the user part is ever typed.
#   VM_STATIC_IP  optional: the fixed address `vm.sh network` gives the VM's
#              host-only adapter (e.g. 172.16.18.253)
#   SSH_KEY    the key to log in with; a .pub file selects that key from the
#              SSH agent, with no private key on disk
#
# THE PASSWORD. The VM is encrypted (Windows 11's vTPM requires it), so every
# vmrun call that opens it needs `-vp`. It is read from trove only when
# needed ($VM_PASSWORD_ENTRY, default antimatter-studios/windows-test-vm) and
# is on vmrun's command line for as long as that call runs -- vmrun has no
# other way to take it.
#
# Setting the VM up: docs/vm-setup.md in fs-windows-test-harness.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VMRUN="${VMRUN:-/Applications/VMware Fusion.app/Contents/Public/vmrun}"
VM_PASSWORD_ENTRY="${VM_PASSWORD_ENTRY:-antimatter-studios/windows-test-vm}"
DOC="https://github.com/antimatter-studios/fs-windows-test-harness/blob/main/docs/vm-setup.md"
FUSION_NETWORKING="/Library/Preferences/VMware Fusion/networking"

if [ -z "${VM_VMX:-}" ] && [ -f "$REPO/.test-env" ]; then
    # Run directly rather than through chore: read the same file.
    while IFS= read -r line; do
        line="${line#export }"
        case "$line" in [A-Za-z_]*=*)
            key="${line%%=*}"; val="${line#*=}"
            val="${val#\"}"; val="${val%\"}"
            [ -n "${!key:-}" ] || export "$key=$val" ;;
        esac
    done < "$REPO/.test-env"
fi

# The agent that holds the VM's key. SSH_KEY is a .pub: the private half is
# served by trove, and a shell whose SSH_AUTH_SOCK points somewhere else
# (launchd's agent, on macOS) offers no key and every ssh here fails as if
# the VM were at fault. See scripts/vm-ssh-agent.sh.
SSH_AUTH_SOCK="$("$REPO/scripts/vm-ssh-agent.sh")"; export SSH_AUTH_SOCK

say()  { printf '%s\n' "$*"; }
fail() { printf 'vm: %s\n' "$1" >&2; shift; for l in "$@"; do printf '    %s\n' "$l" >&2; done; exit 1; }

need_settings() {
    [ -n "${VM_VMX:-}" ]  || fail "VM_VMX is not set -- add the .vmx path to .test-env"
    [ -f "$VM_VMX" ]      || fail "$VM_VMX does not exist"
    [ -n "${VM_HOST:-}" ] || fail "VM_HOST is not set in .test-env -- write at least the user, e.g. VM_HOST=chris@"
    [ -x "$VMRUN" ]       || fail "vmrun not found at $VMRUN -- is VMware Fusion installed?"
}

# Captured, not piped into `grep -q`: under pipefail, grep -q exiting at the
# first match makes vmrun die of SIGPIPE, and the pipe reports "not running"
# for a VM that is -- after which `vmrun start` on it blocks forever.
running() { local list; list="$("$VMRUN" list 2>/dev/null)" || return 1; grep -qxF "$VM_VMX" <<<"$list"; }

password() {
    local pw
    if ! pw="$(trove get password "$VM_PASSWORD_ENTRY" 2>/dev/null)" || [ -z "$pw" ]; then
        fail "cannot read the VM password from trove entry $VM_PASSWORD_ENTRY" \
             "unlock trove in this shell first: eval \"\$(trove unlock <vault> --export)\""
    fi
    printf '%s' "$pw"
}

# vmrun prints start-up chatter (ServiceImpl_Opener, LocationGetRoot) even
# when it succeeds; show it only when the call fails.
vmrun_quiet() {
    local out
    if ! out="$("$VMRUN" -T fusion "$@" 2>&1)"; then
        printf '%s\n' "$out" | grep -vE 'LocationGetRoot|ServiceImpl_Opener|message file path' >&2
        return 1
    fi
    printf '%s\n' "$out" | grep -vE 'LocationGetRoot|ServiceImpl_Opener|message file path' || true
}

# vmrun_quiet in a new session (macOS has no setsid(1); perl's POSIX does it).
# Output goes to a FILE, not a $(...) pipe: the VM process inherits vmrun's
# stdout, and a pipe is not at EOF until every holder exits -- so capturing
# it would wait for the VM to shut down.
vmrun_quiet_detached() {
    local log rc=0
    log="$(mktemp)"
    perl -MPOSIX -e 'POSIX::setsid() or die "setsid: $!\n"; exec @ARGV or die "exec: $!\n"' \
        "$VMRUN" -T fusion "$@" >"$log" 2>&1 </dev/null || rc=$?
    if [ "$rc" -ne 0 ]; then
        grep -vE 'LocationGetRoot|ServiceImpl_Opener|message file path' "$log" >&2 || true
    fi
    rm -f "$log"
    return "$rc"
}

# ssh HOST CMD -- batch mode, the configured key only. accept-new records a
# host key the first time an address is seen and refuses a CHANGED one: this
# VM is only ever reached on a private network from this Mac.
vssh() {
    local host="$1"; shift
    local key_opts=()
    [ -n "${SSH_KEY:-}" ] && key_opts=(-o IdentitiesOnly=yes -i "$SSH_KEY")
    ssh -n -o BatchMode=yes -o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new \
        "${key_opts[@]+"${key_opts[@]}"}" "$host" "$@"
}
answers() { vssh "$1" "echo ok" >/dev/null 2>&1; }

# guest_ps HOST SCRIPT -- run PowerShell in the guest and print its stdout.
# -EncodedCommand (base64 UTF-16LE) so no quoting has to survive two shells.
# Progress is switched off: over a non-interactive SSH session PowerShell
# sends its progress bar ("Preparing modules for first use") to stderr as
# CLIXML, which buries the answer. stderr is kept apart for errors.
guest_ps() {
    local host="$1" enc
    enc="$(printf '%s' "\$ProgressPreference='SilentlyContinue'
$2" | iconv -f UTF-8 -t UTF-16LE | base64 | tr -d '\n')"
    vssh "$host" "powershell -NoProfile -NonInteractive -EncodedCommand $enc" 2>/dev/null | tr -d '\r'
}

user_part() { case "${VM_HOST:-}" in *@*) printf '%s@' "${VM_HOST%@*}" ;; esac; }

# Rewrite VM_HOST in .test-env. Every consumer (the harness, run-matrix.sh,
# these tasks) reads that file, so it is the one place the address lives.
set_vm_host() {
    local new="$1" f="$REPO/.test-env" tmp
    [ "${VM_HOST:-}" = "$new" ] && return 0
    tmp="$(mktemp)"
    if grep -qE '^(export )?VM_HOST=' "$f" 2>/dev/null; then
        awk -v line="VM_HOST=$new" '/^(export )?VM_HOST=/ { print line; next } { print }' "$f" > "$tmp"
    else
        { cat "$f" 2>/dev/null; printf 'VM_HOST=%s\n' "$new"; } > "$tmp"
    fi
    mv "$tmp" "$f"
    say "vm: VM_HOST ${VM_HOST:-(unset)} -> $new (.test-env)"
    VM_HOST="$new"
}

# The guest's address as VMware reports it (VMware Tools, the NAT adapter).
reported_ip() {
    local pw ip
    pw="$(password)"
    ip="$(vmrun_quiet -vp "$pw" getGuestIPAddress "$VM_VMX" -wait | tail -n 1)" || true
    case "$ip" in *[!0-9.]*|'') fail "VMware reported no IPv4 address for the guest (got '$ip')" \
                                    "VMware Tools must be running in Windows. Guide: $DOC" ;; esac
    printf '%s' "$ip"
}

# Every address the guest might answer on, best first, one per line: the
# fixed one (VM_STATIC_IP), what VMware reports (ONE adapter's address, and
# not always the same one), and each adapter's DHCP lease from Fusion's own
# lease files, matched by the MACs in the .vmx. Measured 2026-09-18: VMware
# reported the host-only address while its firewall rule was missing, and a
# discovery that trusted that answer waited on an address that could never
# reply while the NAT one was up.
candidate_ips() {
    local mac lease ip
    [ -n "${VM_STATIC_IP:-}" ] && printf '%s\n' "$VM_STATIC_IP"
    ( reported_ip ) 2>/dev/null && printf '\n'
    sed -nE 's/^ethernet[0-9]+\.generatedAddress = "([0-9a-fA-F:]+)"/\1/p' "$VM_VMX" | while read -r mac; do
        for lease in /var/db/vmware/vmnet-dhcpd-vmnet*.leases; do
            [ -r "$lease" ] || continue
            # The LAST lease block naming this MAC is the current one.
            ip="$(awk -v mac="$(printf '%s' "$mac" | tr 'A-F' 'a-f')" '
                /^lease / { cur = $2 }
                /hardware ethernet/ { m = $3; sub(/;$/, "", m); if (tolower(m) == mac) last = cur }
                END { if (last != "") print last }' "$lease")"
            [ -n "$ip" ] && printf '%s\n' "$ip"
        done
    done
}

# The first candidate that answers SSH, as user@ip; waits up to SECONDS for
# one to appear (a guest that is still booting answers on none of them).
reachable() {  # reachable SECONDS
    local user waited=0 ip
    user="$(user_part)"
    while :; do
        for ip in $(candidate_ips | awk 'NF && !seen[$0]++'); do
            if answers "$user$ip"; then printf '%s%s\n' "$user" "$ip"; return 0; fi
        done
        [ "$waited" -ge "$1" ] && return 1
        sleep 5; waited=$((waited + 5))
    done
}

wait_for_ssh() {  # wait_for_ssh HOST SECONDS
    local waited=0
    until answers "$1"; do
        [ "$waited" -ge "$2" ] && return 1
        sleep 5; waited=$((waited + 5))
    done
}

cmd_status() {
    need_settings
    local state=stopped ssh=down
    running && state=running
    answers "$VM_HOST" && ssh=up
    say "vm: $state, ssh $ssh ($VM_HOST)"
}

cmd_start() {  # start headless if needed; sets STARTED
    STARTED=0
    running && return 0
    local pw; pw="$(password)"
    # IN ITS OWN SESSION. vmrun starts the VM process (vmware-vmx) as its own
    # child, so it inherits our process group -- and chore runs each step in
    # a process group that it signals when the step ends. A vm:up that failed
    # at 15:17:44 on 2026-09-18 powered the VM off in the same second
    # (VMAutomationPowerOff in vmware.log). setsid puts vmrun, and the VM it
    # starts, beyond the reach of whatever runs this script.
    vmrun_quiet_detached -vp "$pw" start "$VM_VMX" nogui >/dev/null || fail "vmrun could not start $VM_VMX"
    STARTED=1
}

# THE FIXED ADDRESS. Everything below exists to make VM_HOST answer.
cmd_network() {
    need_settings
    cmd_start
    local ip="${VM_STATIC_IP:-}"
    [ -n "$ip" ] || fail "VM_STATIC_IP is not set -- add the fixed address to .test-env, e.g. VM_STATIC_IP=172.16.18.253" \
        "Guide: $DOC (section 1)"
    case "$ip" in *[!0-9.]*) fail "VM_STATIC_IP must be an IPv4 address, got '$ip'" ;; esac
    local user; user="$(user_part)"
    if answers "$user$ip"; then set_vm_host "$user$ip"; say "vm: fixed address ok ($user$ip)"; return 0; fi
    local net="${ip%.*}.0"

    # 1. The Mac side: a Fusion network whose subnet holds the address.
    if ! grep -qE "^answer VNET_[0-9]+_HOSTONLY_SUBNET ${net//./\\.}\$" "$FUSION_NETWORKING" 2>/dev/null; then
        fail "no VMware network on this Mac contains $ip ($net/24)" \
             "Fusion > Settings > Network > +: subnet $net, mask 255.255.255.0," \
             "'Connect the host Mac to this network' on, DHCP off. Then add an adapter" \
             "on that network to the VM (VM off: Settings > Add Device > Network Adapter)." \
             "Guide: $DOC"
    fi
    local vnet; vnet="$(sed -nE "s/^answer (VNET_[0-9]+)_HOSTONLY_SUBNET ${net//./\\.}\$/\1/p" "$FUSION_NETWORKING" | head -1)"
    vnet="vmnet${vnet#VNET_}"

    # 2. In through whichever address answers now (the NAT one, usually).
    local via_host via
    via_host="$(reachable 300)" || fail "the guest answers SSH on none of its addresses" \
        "OpenSSH server and key login: $DOC (section 5)"
    via="${via_host#*@}"

    # 3. Windows: find the adapter and set it, or say why not. Sent as
    # -EncodedCommand so no quoting survives two shells.
    local ps
    ps="\$ErrorActionPreference='Stop'
\$target='$ip'; \$via='$via'
\$has = Get-NetIPAddress -AddressFamily IPv4 -IPAddress \$target -ErrorAction SilentlyContinue
if (\$has) { \$if = \$has.InterfaceAlias; \$did = 'present' } else {
  \$viaIf = (Get-NetIPAddress -AddressFamily IPv4 -IPAddress \$via -ErrorAction SilentlyContinue).InterfaceAlias
  \$cands = @(Get-NetAdapter | Where-Object { \$_.Status -eq 'Up' -and \$_.Name -ne \$viaIf })
  if (\$cands.Count -eq 0) { 'NOADAPTER'; exit 0 }
  if (\$cands.Count -gt 1) { 'MANY ' + ((\$cands | ForEach-Object Name) -join ', '); exit 0 }
  \$if = \$cands[0].Name
  Get-NetIPAddress -InterfaceAlias \$if -AddressFamily IPv4 -ErrorAction SilentlyContinue |
    Where-Object { \$_.PrefixOrigin -in 'Manual','Dhcp' } | Remove-NetIPAddress -Confirm:\$false
  Set-NetIPInterface -InterfaceAlias \$if -Dhcp Disabled
  New-NetIPAddress -InterfaceAlias \$if -IPAddress \$target -PrefixLength 24 | Out-Null
  \$did = 'set'
}
for (\$i = 0; \$i -lt 30; \$i++) {
  \$p = Get-NetConnectionProfile -InterfaceAlias \$if -ErrorAction SilentlyContinue
  if (\$p) { break }; Start-Sleep 1
}
# SSH on this address under ANY firewall profile. The adapter has no gateway,
# so Windows calls it an Unidentified network, puts it in the Public profile,
# and does not remember a change to Private across a reboot -- while the
# stock sshd rule allows Private only. Measured 2026-09-18: Private set,
# rebooted, Public again, SSH on the fixed address timed out. So the rule is
# scoped to this address and this subnet instead of to a profile.
\$rule = 'fs-windows-test-harness sshd (host-only)'
Get-NetFirewallRule -DisplayName \$rule -ErrorAction SilentlyContinue | Remove-NetFirewallRule
\$subnet = (\$target -replace '[.][0-9]+\$', '.0') + '/24'
New-NetFirewallRule -DisplayName \$rule -Direction Inbound -Protocol TCP -LocalPort 22 -LocalAddress \$target -RemoteAddress \$subnet -Profile Any -Action Allow | Out-Null
\$did += ', sshd firewall rule'
'OK ' + \$did + ' on ' + \$if"
    local result
    result="$(guest_ps "$via_host" "$ps" | grep -v '^[[:space:]]*$' | tail -n 1)" || true
    case "$result" in
        NOADAPTER) fail "the VM has no network adapter besides the NAT one" \
                        "shut it down (chore vm:down), then in Fusion: VM Settings > Add Device >" \
                        "Network Adapter, on $vnet. Start it and re-run chore vm:network." \
                        "Guide: $DOC (section 3)" ;;
        MANY*)     fail "more than one adapter could be the host-only one: ${result#MANY }" \
                        "remove the extra adapters in Fusion, or set $ip on the right one by hand." \
                        "Guide: $DOC (section 4)" ;;
        OK*)       ;;
        *)         fail "setting $ip inside Windows failed: $result" "Guide: $DOC (section 4)" ;;
    esac

    wait_for_ssh "$user$ip" 60 || fail "$ip is set in Windows (${result#OK }) but SSH does not answer there" \
        "check the sshd firewall rule covers the Private profile. Guide: $DOC (section 5)"
    set_vm_host "$user$ip"
    say "vm: fixed address ${result#OK } ($user$ip)"
}

# THE ADDRESS, DISCOVERED. VMware reports the NAT address, which moves; a
# host-only adapter with a static address (vm.sh network) does not. So: ask
# VMware, log in there, and prefer any other address of the guest's that
# answers from this Mac. Prints user@address.
cmd_address() {
    need_settings
    running || fail "the VM is not running -- chore vm:up"
    [ -n "$(user_part)" ] || fail "VM_HOST has no user part -- write e.g. VM_HOST=chris@ in .test-env"
    local addr
    addr="$(reachable 300)" || fail "the guest answers SSH on none of its addresses: $(candidate_ips | awk 'NF && !seen[$0]++' | paste -sd, -)" \
        "OpenSSH server and key login: $DOC (section 5)"
    printf '%s\n' "$addr"
}

cmd_up() {
    need_settings
    cmd_start
    local started=$STARTED addr
    # Fast path: the address .test-env already names still answers.
    if [ "$started" = 0 ] && answers "$VM_HOST"; then
        say "vm: running, ssh up ($VM_HOST)"; return 0
    fi
    addr="$(cmd_address)"
    set_vm_host "$addr"
    wait_for_ssh "$VM_HOST" 60 || fail "$VM_HOST did not answer SSH" "Guide: $DOC"
    case "${VM_HOST#*@}" in
        "${VM_STATIC_IP:-none}") ;;
        *) [ -n "${VM_STATIC_IP:-}" ] && say "vm: note -- on the NAT address, which moves; chore vm:network sets $VM_STATIC_IP" ;;
    esac
    if [ "$started" = 1 ]; then say "vm: started, ssh up ($VM_HOST)"; else say "vm: running, ssh up ($VM_HOST)"; fi
}

cmd_down() {
    need_settings
    running || { say "vm: already stopped"; return 0; }
    local pw; pw="$(password)"
    # `soft` asks Windows to shut down as the Start menu would; a hard stop
    # is pulling the plug on a guest that may be mid-write.
    vmrun_quiet -vp "$pw" stop "$VM_VMX" soft >/dev/null || fail "vmrun could not stop $VM_VMX"
    say "vm: stopped"
}

case "${1:-}" in
    status)  cmd_status ;;
    up)      cmd_up ;;
    down)    cmd_down ;;
    network) cmd_network ;;
    address) cmd_address ;;
    *) echo "usage: scripts/vm.sh status|up|down|address|network" >&2; exit 2 ;;
esac
