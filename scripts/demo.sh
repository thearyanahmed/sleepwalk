#!/usr/bin/env bash
# sleepwalk live-migration demo driver.
#
# Drives the stateful in-RAM `ramstate` app inside a microVM and migrates it
# A -> B so you can watch its memory (a counter) survive the move. Host addresses
# come from .env (REMOTE_A_HOST / REMOTE_B_HOST); nothing host-specific is baked
# in. Two hosts must already be CPU-compatible (see ADR-004) and have the bridge
# + VXLAN fabric reachable; `up` (re)establishes the fabric, daemons and a fresh
# VM for you.
#
#   scripts/demo.sh up        reset to a fresh app VM on A at 10.200.0.2, DNAT ready
#   scripts/demo.sh watch     (terminal 1) POST /inc in a loop, print the counter
#   scripts/demo.sh migrate   (terminal 2) migrate the VM A -> B once
#   scripts/demo.sh state     print the app's current /state (full, with log)
#   scripts/demo.sh status    show where the VM lives (A vs B)
#
# A restored VM gets a new id on the target and is not re-migratable by the same
# id, so a run is one-way A -> B. Run `up` again for another A -> B demo.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

ENV_FILE="$SLEEPWALK_ROOT/.env"
[[ -f "$ENV_FILE" ]] || _die "no .env — set REMOTE_A_HOST / REMOTE_B_HOST"
set -a
# shellcheck disable=SC1090
source "$ENV_FILE"
set +a
A="${REMOTE_A_HOST:?set REMOTE_A_HOST in .env}"
B="${REMOTE_B_HOST:?set REMOTE_B_HOST in .env}"

HOST="$SLEEPWALK_ROOT/scripts/host.sh"
GUEST_IP=10.200.0.2   # first VM after a daemon reset lands here
APP_PORT=18080        # public host port DNAT'd to the guest's :8000
DATA_PORT=9000        # migration transfer port
VM_FILE=/tmp/sw-demo-vm

# ssh to a host with a timeout so a flaky link can't wedge the driver.
sh_host() { local label="$1"; shift; timeout 45 "$HOST" "$label" ssh "$@" 2>/dev/null; }

reset_daemon() { # label host_id
    sh_host "$1" "pkill -x hostd 2>/dev/null; sleep 2; sudo pkill -9 -f '[f]irecracker' 2>/dev/null; sleep 1; for t in \$(ip -o link show|grep -oE 'sw-tap[0-9]+'|sort -u); do sudo ip link del \$t 2>/dev/null; done; rm -rf /tmp/sleepwalk-vm-* 2>/dev/null; cd sleepwalk && setsid nohup target/release/hostd daemon 0.0.0.0:8080 $2 >/tmp/hostd.log 2>&1 </dev/null & sleep 2; true" >/dev/null || true
}
ensure_net() { # label up-arg local-ip remote-ip
    sh_host "$1" "cd sleepwalk; sudo scripts/net-host.sh up $2 >/dev/null 2>&1; sudo scripts/net-host.sh vxlan $3 $4 >/dev/null 2>&1; true" >/dev/null || true
}
ensure_dnat() { # label
    sh_host "$1" "
        sudo iptables -t nat -N SW_DNAT 2>/dev/null || true
        sudo iptables -t nat -C PREROUTING -p tcp --dport $APP_PORT -j SW_DNAT 2>/dev/null || sudo iptables -t nat -A PREROUTING -p tcp --dport $APP_PORT -j SW_DNAT
        sudo iptables -t nat -C POSTROUTING -d 10.200.0.0/24 -p tcp --dport 8000 -j MASQUERADE 2>/dev/null || sudo iptables -t nat -A POSTROUTING -d 10.200.0.0/24 -p tcp --dport 8000 -j MASQUERADE
        sudo iptables -t nat -F SW_DNAT; sudo iptables -t nat -A SW_DNAT -p tcp --dport $APP_PORT -j DNAT --to-destination $GUEST_IP:8000
        true" >/dev/null || true
}

cmd_up() {
    _log "resetting daemons (A=gateway .1, B=peer .254) + overlay + DNAT"
    reset_daemon a server_a
    reset_daemon b server_b
    ensure_net a "10.200.0.1/24" "$A" "$B"
    ensure_net b "none" "$B" "$A"
    ensure_dnat a
    ensure_dnat b
    sleep 3
    _log "spawning app VM on A"
    local vm
    vm=$(curl -s -m45 -X POST "http://$A:8080/vms/spawn?mib=256&net=1" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("vm",""))' 2>/dev/null)
    [[ -n "$vm" ]] || _die "spawn failed"
    echo "$vm" >"$VM_FILE"
    _log "vm $vm — waiting for the app to answer on http://$A:$APP_PORT"
    local i
    for i in $(seq 1 20); do
        [[ -n "$(curl -s -m3 "http://$A:$APP_PORT/state" 2>/dev/null)" ]] && {
            _log "ready."
            echo
            echo "  terminal 1:  scripts/demo.sh watch"
            echo "  terminal 2:  scripts/demo.sh migrate"
            return 0
        }
        sleep 2
    done
    _die "app did not become reachable (check the overlay/DNAT)"
}

cmd_watch() {
    echo "watching http://$A:$APP_PORT/inc — counter from VM RAM (Ctrl-C to stop)"
    while true; do
        curl -s -m2 -X POST "http://$A:$APP_PORT/inc" || echo "(no response — migrating?)"
        echo
        sleep 0.3
    done
}

cmd_migrate() {
    [[ -f "$VM_FILE" ]] || _die "no VM recorded — run 'demo.sh up' first"
    local vm
    vm=$(cat "$VM_FILE")
    _log "migrating $vm  A -> B"
    curl -s -m10 -X POST "http://$B:8080/migrate/recv?listen=0.0.0.0:$DATA_PORT" >/dev/null
    local resp
    resp=$(curl -s -m90 -X POST "http://$A:8080/migrate/send?vm=$vm&to=$B:$DATA_PORT")
    echo "$resp"
    echo
    _log "done — VM now on B; the same counter continues via http://$A:$APP_PORT"
    _log "run 'demo.sh up' to reset for another A -> B run"
}

cmd_state() { curl -s -m6 "http://$A:$APP_PORT/state"; echo; }

cmd_status() {
    echo -n "A: "; curl -s -m5 "http://$A:8080/status" | python3 -c 'import sys,json;d=json.load(sys.stdin);print(len(d["vms"]),"vm(s)")' 2>/dev/null || echo unreachable
    echo -n "B: "; curl -s -m5 "http://$B:8080/status" | python3 -c 'import sys,json;d=json.load(sys.stdin);print(len(d["vms"]),"vm(s)")' 2>/dev/null || echo unreachable
}

case "${1:-}" in
    up) cmd_up ;;
    watch) cmd_watch ;;
    migrate) cmd_migrate ;;
    state) cmd_state ;;
    status) cmd_status ;;
    *) echo "usage: demo.sh {up | watch | migrate | state | status}" >&2; exit 2 ;;
esac
