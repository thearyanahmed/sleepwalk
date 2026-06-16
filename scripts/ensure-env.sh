#!/usr/bin/env bash
# Shared environment + helpers for the live-migration demo scripts.
#
# Source it from the other scripts: `source "$(dirname "$0")/ensure-env.sh"`.
# Run it directly to just validate that .env is present and usable.
#
# Exposes: A, B (host addresses from .env), GUEST_IP, APP_PORT, DATA_PORT, and the
# helpers sh_host() and vm_at_guest_ip().

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

ENV_FILE="$SLEEPWALK_ROOT/.env"
[[ -f "$ENV_FILE" ]] || _die "no .env — copy .env.example to .env and set REMOTE_A_HOST / REMOTE_B_HOST"
set -a
# shellcheck disable=SC1090
source "$ENV_FILE"
set +a
A="${REMOTE_A_HOST:?set REMOTE_A_HOST in .env}"
B="${REMOTE_B_HOST:?set REMOTE_B_HOST in .env}"

HOST="$SLEEPWALK_ROOT/scripts/host.sh"
GUEST_IP=10.200.0.2   # the demo VM's address (first VM after a daemon reset)
APP_PORT=18080        # public host port DNAT'd to the guest's :8000
DATA_PORT=9000        # migration transfer port

# ssh to host label (a|b) with a timeout so a flaky link can't wedge a script.
sh_host() { local label="$1"; shift; timeout 45 "$HOST" "$label" ssh "$@" 2>/dev/null; }

# id of the live demo VM (the one at GUEST_IP) on daemon $1 (a base URL), else
# empty — discovered from metrics, so no temp file and it survives reboots.
vm_at_guest_ip() {
    curl -s -m5 "$1/metrics" 2>/dev/null \
        | grep "ip=\"$GUEST_IP\".*} 1" \
        | grep -oE 'vm_id="[^"]+"' | head -1 | sed 's/vm_id="//;s/"//'
}

# When executed directly (not sourced) just report the env is good.
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    _log "env ok — A=$A  B=$B  (guest $GUEST_IP, app :$APP_PORT)"
fi
