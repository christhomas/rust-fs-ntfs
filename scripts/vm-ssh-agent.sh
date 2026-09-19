#!/usr/bin/env bash
# vm-ssh-agent.sh -- print the SSH_AUTH_SOCK that can log in to the VM.
#
# SSH_KEY in .test-env is a .pub file: it names WHICH key to offer, and the
# private half never touches the disk -- it is served by trove's agent. That
# only works if $SSH_AUTH_SOCK points at THAT agent, and on macOS it usually
# does not: a shell started outside the unlocked session inherits launchd's
# agent (/var/run/com.apple.launchd.*/Listeners), which serves no keys at all.
#
# WHAT THAT LOOKS LIKE IF NOBODY CHECKS. A matrix run on 2026-09-18 failed 164
# times with `scp: Connection closed`, each preceded by
#
#   WARNING: UNPROTECTED PRIVATE KEY FILE!
#
# -- ssh, finding no agent key, falling back to reading the .pub as a private
# key and refusing it for its 0644 mode. Every failure named the VM, and none
# of them was the VM's fault: the same scp worked with SSH_AUTH_SOCK set to
# trove's socket. Diagnosing that a second time costs more than this file.
#
# Usage:  SSH_AUTH_SOCK="$(scripts/vm-ssh-agent.sh)"; export SSH_AUTH_SOCK
#
# It prints the current socket when that one already serves the key, so a
# machine that keeps its keys somewhere else is left alone; trove is the
# fallback, not the assumption. It prints nothing and exits 0 when it cannot
# find an agent serving the key -- the caller's ssh then fails with its own
# error, which is a better message than anything this could invent.
set -uo pipefail

# Does this agent listing offer this key? The listing is `ssh-add -L` output
# (`<algo> <base64> <comment>` per line); the key is the first two fields of a
# .pub file. Comments differ between the file and the agent -- trove names its
# entries after the vault path -- so only the algorithm and the blob are
# compared.
agent_serves() {
    local want_algo="$1" want_blob="$2" algo blob
    while read -r algo blob _; do
        [ "$algo" = "$want_algo" ] && [ "$blob" = "$want_blob" ] && return 0
    done
    return 1
}

main() {
    REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
    cd "$REPO" || exit 0
    # shellcheck disable=SC1091
    [ -f .test-env ] && . ./.test-env

    # No key named, or it is a private key on disk: SSH_AUTH_SOCK is not what
    # decides the login, so leave the environment as it is.
    case "${SSH_KEY:-}" in
        '') printf '%s' "${SSH_AUTH_SOCK:-}"; return 0 ;;
        *.pub) ;;
        *) printf '%s' "${SSH_AUTH_SOCK:-}"; return 0 ;;
    esac
    [ -f "$SSH_KEY" ] || { printf '%s' "${SSH_AUTH_SOCK:-}"; return 0; }

    read -r algo blob _ < "$SSH_KEY"

    if [ -n "${SSH_AUTH_SOCK:-}" ] && [ -S "$SSH_AUTH_SOCK" ] \
       && ssh-add -L 2>/dev/null | agent_serves "$algo" "$blob"; then
        printf '%s' "$SSH_AUTH_SOCK"
        return 0
    fi

    command -v trove >/dev/null 2>&1 || { printf '%s' "${SSH_AUTH_SOCK:-}"; return 0; }
    local sock
    sock="$(trove ssh-agent socket 2>/dev/null)"
    if [ -n "$sock" ] && [ -S "$sock" ] \
       && SSH_AUTH_SOCK="$sock" ssh-add -L 2>/dev/null | agent_serves "$algo" "$blob"; then
        printf '%s' "$sock"
        return 0
    fi

    # Nothing serves it. Say so on stderr -- stdout is a value a caller
    # assigns -- because "the vault is locked" is the likeliest reason and it
    # is fixed in one command.
    echo "vm-ssh-agent: no agent is serving $SSH_KEY." >&2
    echo "              trove unlock \$HOME/icloud-christhomas/christhomas.kdbx --env" >&2
    printf '%s' "${SSH_AUTH_SOCK:-}"
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    main "$@"
fi
