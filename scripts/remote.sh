#!/usr/bin/env bash
# Sync this repo to a remote Linux box (a droplet / Pi with the Linux-only
# capabilities macOS lacks) and run things on it over SSH. Connection details
# live in a gitignored .env (copy .env.example -> .env and fill it); nothing
# host-specific or secret is committed.
#
# Usage:
#   scripts/remote.sh sync                 rsync the repo to the remote
#   scripts/remote.sh ssh [cmd...]         shell in, or run a one-off command
#   scripts/remote.sh setup [args...]      sync, then run scripts/setup.sh there
#   scripts/remote.sh run <just-target>    sync, then `just <target>` there
#
# Auth: REMOTE_SSH_KEY (preferred) or REMOTE_PASSWORD (needs `sshpass`).

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

ENV_FILE="$SLEEPWALK_ROOT/.env"
[[ -f "$ENV_FILE" ]] || _die "no .env — copy .env.example to .env and fill it in"

# Load .env (export every assignment for child processes).
set -a
# shellcheck disable=SC1090
source "$ENV_FILE"
set +a

# Host selection: `remote.sh --host b <cmd>` targets the REMOTE_B_* vars (the
# second droplet); the default is host A (the REMOTE_* vars).
if [[ "${1:-}" == "--host" ]]; then
    case "${2:-}" in
        a | A) ;;
        b | B)
            REMOTE_HOST="${REMOTE_B_HOST:-}"
            REMOTE_USER="${REMOTE_B_USER:-}"
            REMOTE_PORT="${REMOTE_B_PORT:-}"
            REMOTE_PATH="${REMOTE_B_PATH:-}"
            REMOTE_SSH_KEY="${REMOTE_B_SSH_KEY:-}"
            REMOTE_PASSWORD="${REMOTE_B_PASSWORD:-}"
            ;;
        *) _die "unknown --host '${2:-}' (use a or b)" ;;
    esac
    shift 2
fi

: "${REMOTE_HOST:?set REMOTE_HOST (or REMOTE_B_HOST with --host b) in .env}"
REMOTE_USER="${REMOTE_USER:-root}"
REMOTE_PORT="${REMOTE_PORT:-22}"
REMOTE_PATH="${REMOTE_PATH:-sleepwalk}"
TARGET="$REMOTE_USER@$REMOTE_HOST"

# accept-new: trust the host key on first contact, then pin it (real MITM
# protection on every later connection) — unlike the throwaway dev-vm's =no.
SSH_OPTS=(-o StrictHostKeyChecking=accept-new -p "$REMOTE_PORT")

if [[ -n "${REMOTE_SSH_KEY:-}" ]]; then
    key="${REMOTE_SSH_KEY/#\~/$HOME}"   # expand a leading ~
    [[ -f "$key" ]] || _die "REMOTE_SSH_KEY not found: $key"
    SSH_OPTS+=(-i "$key")
    SSH=(ssh "${SSH_OPTS[@]}")
elif [[ -n "${REMOTE_PASSWORD:-}" ]]; then
    _need sshpass "install it (macOS: brew install sshpass; Debian: apt install sshpass)"
    _warn "password auth in use — after first login run 'ssh-copy-id' and switch to REMOTE_SSH_KEY"
    # -e reads the password from $SSHPASS, so it never appears in argv / ps.
    export SSHPASS="$REMOTE_PASSWORD"
    SSH=(sshpass -e ssh "${SSH_OPTS[@]}")
else
    _die "set REMOTE_SSH_KEY or REMOTE_PASSWORD in .env"
fi

cmd_ssh() { "${SSH[@]}" "$TARGET" "$@"; }

cmd_sync() {
    _need rsync
    _need git
    # Let git decide what is local-only. `git ls-files -o -i --exclude-standard`
    # lists every ignored path — honoring the repo .gitignore, the user's global
    # ignore, and .git/info/exclude — so build output, fetched artifacts,
    # snapshot dirs, every .env, and any private local file are all skipped
    # without this script having to name a single one of them. If the
    # enumeration fails we abort rather than risk pushing something private.
    local exfile
    exfile="$(mktemp)"
    # shellcheck disable=SC2064
    trap "rm -f '$exfile'" RETURN
    ( cd "$SLEEPWALK_ROOT" && git ls-files -o -i --exclude-standard --directory ) >"$exfile" \
        || _die "could not enumerate ignored files (is this a git repo?)"

    _log "syncing repo -> $TARGET:$REMOTE_PATH/"
    # Plain --delete (not --delete-excluded): the remote is mirrored for tracked
    # files, but its own ignored build dir (target/) is left intact for fast
    # incremental rebuilds. .git and any .env are excluded belt-and-suspenders.
    rsync -az --delete -e "${SSH[*]}" \
        --exclude '.git' \
        --exclude '.env' \
        --exclude '*.env' \
        --exclude-from="$exfile" \
        "$SLEEPWALK_ROOT/" "$TARGET:$REMOTE_PATH/"
    _log "sync done"
}

cmd_setup() {
    cmd_sync
    _log "running scripts/setup.sh on $TARGET"
    cmd_ssh "cd '$REMOTE_PATH' && scripts/setup.sh $*"
}

cmd_run() {
    [[ $# -ge 1 ]] || _die "usage: remote.sh run <just-target> [args...]"
    cmd_sync
    _log "running 'just $*' on $TARGET"
    # A non-interactive ssh shell does not source ~/.profile, so cargo/just in
    # ~/.cargo/bin are off PATH; source the cargo env first. \$HOME is escaped so
    # it expands on the remote, not here.
    cmd_ssh "cd '$REMOTE_PATH' && { [ -f \"\$HOME/.cargo/env\" ] && . \"\$HOME/.cargo/env\"; }; just $*"
}

case "${1:-}" in
    sync)  cmd_sync ;;
    ssh)   shift; cmd_ssh "$@" ;;
    setup) shift; cmd_setup "$@" ;;
    run)   shift; cmd_run "$@" ;;
    *)     _die "usage: remote.sh {sync | ssh [cmd] | setup [args] | run <just-target>}" ;;
esac
