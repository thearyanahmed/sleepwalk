#!/usr/bin/env bash
# Prepare a fresh demo environment, end to end:
#   - reset both daemons (so the VM lands at 10.200.0.2),
#   - (re)build the bridge + VXLAN overlay (A = gateway .1, B = peer .254),
#   - point the DNAT chain at the guest,
#   - spawn the stateful app VM (ramstate) on A and wait until it answers.
# Run once before a demo, or anytime to reset for another run.

source "$(dirname "$0")/ensure-env.sh"

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

_log "resetting daemons (A=gateway .1, B=peer .254) + overlay + DNAT"
reset_daemon a server_a
reset_daemon b server_b
ensure_net a "10.200.0.1/24" "$A" "$B"
ensure_net b "none" "$B" "$A"
ensure_dnat a
ensure_dnat b
sleep 3

_log "spawning app VM on A"
vm=$(curl -s -m45 -X POST "http://$A:8080/vms/spawn?mib=256&net=1" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("vm",""))' 2>/dev/null)
[[ -n "$vm" ]] || _die "spawn failed"
_log "vm $vm — waiting for the app on http://$A:$APP_PORT"
for _ in $(seq 1 20); do
    [[ -n "$(curl -s -m3 "http://$A:$APP_PORT/state" 2>/dev/null)" ]] && {
        _log "ready."
        echo
        echo "  terminal 1:  ./scripts/long-process.sh"
        echo "  terminal 2:  ./scripts/status.sh   (live)   and   ./scripts/migrate.sh"
        exit 0
    }
    sleep 2
done
_die "app did not become reachable (check the overlay / DNAT)"
