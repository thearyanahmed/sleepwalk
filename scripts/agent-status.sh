#!/usr/bin/env bash
# Live agent status: which droplet currently holds the VM (from each host daemon's
# /metrics) and how many turns the agent has served (from the agent's own HTTP).
# Prints only on change — so a migration shows up as the location flipping A<->B
# while the turn count keeps climbing across it.
source "$(dirname "$0")/ensure-env.sh"

host_with_vm() {
    local label ip
    for label in A B; do
        ip=$([[ "$label" == A ]] && echo "$A" || echo "$B")
        if curl -s -m3 "http://$ip:8080/metrics" 2>/dev/null \
            | grep -qE "sleepwalk_vm_info\{[^}]*ip=\"$GUEST_IP\"[^}]*\} 1"; then
            echo "$label ($ip)"
            return
        fi
    done
    echo "none"
}

_log "live agent status (Ctrl-C to stop) — location flips on migration; turns keep climbing"
prev=""
while true; do
    loc=$(host_with_vm)
    turns=$(curl -s -m3 "http://$A:$APP_PORT/" 2>/dev/null \
        | python3 -c 'import sys,json; print(json.load(sys.stdin).get("turns","?"))' 2>/dev/null || echo "?")
    line="VM on $loc | turns served: $turns"
    if [[ "$line" != "$prev" ]]; then
        printf '[%s] %s\n' "$(date +%H:%M:%S)" "$line"
        prev="$line"
    fi
    sleep 2
done
