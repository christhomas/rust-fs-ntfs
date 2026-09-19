#!/usr/bin/env bash
# Tests for agent_serves in scripts/vm-ssh-agent.sh -- does this agent offer
# the key .test-env named?
#
# Getting it wrong in either direction is expensive: a false yes leaves
# SSH_AUTH_SOCK pointing at an agent with no key and every scenario fails at
# `ship-to-vm` blaming the VM; a false no sends a machine that keeps its keys
# elsewhere to trove for a socket it does not need.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# shellcheck source=../../scripts/vm-ssh-agent.sh
. "$ROOT/scripts/vm-ssh-agent.sh"

pass=0; fail=0

BLOB=AAAAB3NzaC1yc2EAAAADAQABAAABAQDPxxxxxxxxxxxxxxxxxxxxxxxxxxxx
OTHER=AAAAC3NzaC1lZDI1NTE5AAAAIGyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy

check() {
    local name="$1" listing="$2" algo="$3" blob="$4" want="$5" got=no
    printf '%s\n' "$listing" | agent_serves "$algo" "$blob" && got=yes
    if [ "$got" = "$want" ]; then
        pass=$((pass + 1)); printf '  ok    %s\n' "$name"
    else
        fail=$((fail + 1)); printf '  FAIL  %s\n        got:  %s\n        want: %s\n' "$name" "$got" "$want"
    fi
}

printf 'agent_serves\n'

check "the only key" \
    "ssh-rsa $BLOB comment" ssh-rsa "$BLOB" yes

# What trove's agent actually looks like: ten keys, the wanted one in the
# middle, and every comment naming a vault path rather than the .pub file.
check "one of many, vault comments" \
    "$(printf 'ssh-ed25519 %s Semdatex/github:id_ed25519\nssh-rsa %s antimatter-studios/ssh - root@s1:id_rsa\nssh-ed25519 %s Recycle Bin/ssh' "$OTHER" "$BLOB" "$OTHER")" \
    ssh-rsa "$BLOB" yes

# launchd's agent on this Mac: reachable, and serving nothing.
check "empty agent" "" ssh-rsa "$BLOB" no

# `ssh-add -L` says this when there is no key; it must not be read as a key.
check "the no-identities line" \
    "The agent has no identities." ssh-rsa "$BLOB" no

check "a different key only" \
    "ssh-ed25519 $OTHER other" ssh-rsa "$BLOB" no

# Same blob under another algorithm name is not the same key.
check "right blob, wrong algorithm" \
    "ssh-dss $BLOB comment" ssh-rsa "$BLOB" no

# A blob that merely starts the same must not match: comparison is whole-field.
check "prefix of the blob" \
    "ssh-rsa ${BLOB%??} comment" ssh-rsa "$BLOB" no

# A key with no comment is still a key.
check "no comment field" \
    "ssh-rsa $BLOB" ssh-rsa "$BLOB" yes

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
