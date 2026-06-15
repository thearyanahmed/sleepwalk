#!/usr/bin/env bash
# Sync this repo to a remote Linux box and run things on it over SSH. Pure
# transport: the connection comes from the environment, so any caller can point
# it at any host — `scripts/host.sh` resolves a label from .env, or set the vars
# directly. This script knows nothing about which host it is talking to.
#
# Connection (env):
#   RHOST   (required)   IP or hostname
#   RUSER   (root)       ssh user
#   RPORT   (22)         ssh port
#   RPATH   (sleepwalk)  destination dir under the remote user's home
#   RKEY                 path to a private key (preferred), OR
#   RPASS                a password (needs `sshpass`)
#
# Usage:
#   RHOST=1.2.3.4 RPASS=… scripts/remote.sh sync          rsync the repo over
#   scripts/remote.sh ssh [cmd...]                        shell in / run a command
#   scripts/remote.sh setup [args...]                     sync, then scripts/setup.sh
#   scripts/remote.sh run <just-target> [args...]         sync, then `just <target>`
#   scripts/remote.sh node-exporter                       install + start node_exporter (:9100)

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

RHOST="${RHOST:?RHOST not set — set RHOST/RUSER/… directly or use scripts/host.sh <a|b>}"
RUSER="${RUSER:-root}"
RPORT="${RPORT:-22}"
RPATH="${RPATH:-sleepwalk}"
RKEY="${RKEY:-}"
RPASS="${RPASS:-}"
TARGET="$RUSER@$RHOST"

# accept-new: trust the host key on first contact, then pin it (MITM protection
# on every later connection) — unlike the throwaway dev-vm's =no.
SSH_OPTS=(-o StrictHostKeyChecking=accept-new -p "$RPORT")

if [[ -n "$RKEY" ]]; then
    key="${RKEY/#\~/$HOME}" # expand a leading ~
    [[ -f "$key" ]] || _die "RKEY not found: $key"
    SSH_OPTS+=(-i "$key")
    SSH=(ssh "${SSH_OPTS[@]}")
elif [[ -n "$RPASS" ]]; then
    _need sshpass "install it (macOS: brew install sshpass; Debian: apt install sshpass)"
    _warn "password auth in use — after first login run 'ssh-copy-id' and switch to RKEY"
    # -e reads the password from $SSHPASS, so it never appears in argv / ps.
    export SSHPASS="$RPASS"
    SSH=(sshpass -e ssh "${SSH_OPTS[@]}")
else
    _die "set RKEY or RPASS for $TARGET"
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
    (cd "$SLEEPWALK_ROOT" && git ls-files -o -i --exclude-standard --directory) >"$exfile" \
        || _die "could not enumerate ignored files (is this a git repo?)"

    _log "syncing repo -> $TARGET:$RPATH/"
    # Plain --delete (not --delete-excluded): the remote is mirrored for tracked
    # files, but its own ignored build dir (target/) is left intact for fast
    # incremental rebuilds. .git and any .env are excluded belt-and-suspenders.
    rsync -az --delete -e "${SSH[*]}" \
        --exclude '.git' \
        --exclude '.env' \
        --exclude '*.env' \
        --exclude-from="$exfile" \
        "$SLEEPWALK_ROOT/" "$TARGET:$RPATH/"
    _log "sync done"
}

cmd_setup() {
    cmd_sync
    _log "running scripts/setup.sh on $TARGET"
    cmd_ssh "cd '$RPATH' && scripts/setup.sh $*"
}

cmd_run() {
    [[ $# -ge 1 ]] || _die "usage: remote.sh run <just-target> [args...]"
    cmd_sync
    _log "running 'just $*' on $TARGET"
    # A non-interactive ssh shell does not source ~/.profile, so cargo/just in
    # ~/.cargo/bin are off PATH; source the cargo env first. \$HOME is escaped so
    # it expands on the remote, not here.
    cmd_ssh "cd '$RPATH' && { [ -f \"\$HOME/.cargo/env\" ] && . \"\$HOME/.cargo/env\"; }; just $*"
}

# Version of node_exporter to install on hosts for machine-resource metrics.
NODE_EXPORTER_VERSION="1.8.2"

# Install (if missing) and (re)start prometheus node_exporter on the host,
# listening on 0.0.0.0:9100 so the Prometheus stack can scrape machine
# resources (CPU, memory, disk, net). Idempotent; safe to re-run.
cmd_node_exporter() {
    _log "installing + starting node_exporter $NODE_EXPORTER_VERSION on $TARGET"
    cmd_ssh "NODE_EXPORTER_VERSION='$NODE_EXPORTER_VERSION' bash -s" <<'REMOTE'
set -euo pipefail
case "$(uname -m)" in
    x86_64) a=amd64 ;;
    aarch64) a=arm64 ;;
    *) echo "unsupported arch $(uname -m)" >&2; exit 1 ;;
esac
bin="$HOME/.local/bin/node_exporter"
mkdir -p "$HOME/.local/bin"
if [ ! -x "$bin" ]; then
    tmp="$(mktemp -d)"
    url="https://github.com/prometheus/node_exporter/releases/download/v${NODE_EXPORTER_VERSION}/node_exporter-${NODE_EXPORTER_VERSION}.linux-${a}.tar.gz"
    echo "downloading $url"
    curl -fsSL "$url" | tar xz -C "$tmp" --strip-components=1
    install -m755 "$tmp/node_exporter" "$bin"
    rm -rf "$tmp"
fi
# -x: exact match, never the ssh shell running this.
pkill -x node_exporter 2>/dev/null || true
nohup "$bin" --web.listen-address=0.0.0.0:9100 >/tmp/node_exporter.log 2>&1 &
sleep 1
echo "node_exporter up: $(curl -fsS localhost:9100/metrics | head -1)"
REMOTE
}

case "${1:-}" in
    sync) cmd_sync ;;
    ssh)
        shift
        cmd_ssh "$@"
        ;;
    setup)
        shift
        cmd_setup "$@"
        ;;
    run)
        shift
        cmd_run "$@"
        ;;
    node-exporter) cmd_node_exporter ;;
    *) _die "usage: remote.sh {sync | ssh [cmd] | setup [args] | run <just-target> | node-exporter}" ;;
esac
