#!/usr/bin/env bash
# Agent demo (O6): boot a microVM running a coding agent (aider) on a free model
# endpoint, then migrate it A->B mid-session during an idle gap between turns. The
# agent's in-RAM state + working tree ride the snapshot; it finishes the task on
# the new host, none the wiser.
#
# Prereqs:
#   - .env has AGENT_API_KEY (a free Groq key — console.groq.com), and the usual
#     REMOTE_A_*/REMOTE_B_* blocks.
#   - the agent rootfs is built on both hosts:  just agent-rootfs  (run on each).
#
# Sequence:
#   ./scripts/start-agent.sh           # reset daemons w/ agent env, net up, spawn
#   ./scripts/start-agent.sh watch     # live-tail the agent's turn log
#   ./scripts/migrate.sh               # move it A->B during an idle gap
source "$(dirname "$0")/ensure-env.sh"

# Relative to the daemon's CWD (it runs from ~/sleepwalk — see reset_agent_daemon).
ROOTFS="${AGENT_ROOTFS:-images/artifacts/agent-rootfs-x86_64.ext4}"
MODEL="${AGENT_MODEL:-groq/llama-3.3-70b-versatile}"
GAP="${AGENT_GAP_SECS:-25}"
MIB="${AGENT_MIB:-1024}"
[[ -n "${AGENT_API_KEY:-}" ]] || _die "set AGENT_API_KEY in .env (free Groq key)"

# guestd logs the agent's stdout (turn markers + aider output) to the FC log; the
# daemon writes one per VM under /tmp. Tail whichever host currently holds the VM.
watch_logs() {
    _log "tailing agent turn log (Ctrl-C to stop)"
    while true; do
        for h in a b; do
            timeout 8 "$HOST" "$h" ssh "tail -n 40 /tmp/sleepwalk-vm-*/fc.log 2>/dev/null | grep -aE '\[agent\]|\[aider\]|TURN' | tail -n 15" 2>/dev/null
        done
        sleep 3
    done
}

# Launch the daemon with the agent env so registry::spawn hands AGENT_API_KEY to
# the guest over the Secrets vsock message, and boots the agent rootfs. The key is
# written to a 0600 file the launch sources (kept out of the process argv / ps).
reset_agent_daemon() { # label host_id
    timeout 20 "$HOST" "$1" ssh "
        pkill -x hostd 2>/dev/null; sleep 2
        sudo pkill -9 -f '[f]irecracker' 2>/dev/null; sleep 1
        for t in \$(ip -o link show|grep -oE 'sw-tap[0-9]+'|sort -u); do sudo ip link del \$t 2>/dev/null; done
        rm -rf /tmp/sleepwalk-vm-* 2>/dev/null
        umask 077
        cat > ~/.sleepwalk-agent.env <<EOF
export AGENT_API_KEY='$AGENT_API_KEY'
export AGENT_MODEL='$MODEL'
export AGENT_GAP_SECS='$GAP'
export SLEEPWALK_ROOTFS='$ROOTFS'
EOF
        cd sleepwalk && set -a && . ~/.sleepwalk-agent.env && set +a && \
            setsid nohup target/release/hostd daemon 0.0.0.0:8080 $2 >/tmp/hostd.log 2>&1 </dev/null &
        sleep 2; true" >/dev/null 2>&1 || true
}
wait_healthz() { # ip
    local i
    for i in $(seq 1 20); do
        [[ "$(curl -s -m3 "http://$1:8080/healthz" 2>/dev/null)" == "ok" ]] && return 0
        sleep 1
    done
    _die "daemon at $1 did not come up"
}
ensure_net() { # label up-arg local-ip remote-ip
    sh_host "$1" "cd sleepwalk; sudo scripts/net-host.sh up $2 >/dev/null 2>&1; sudo scripts/net-host.sh vxlan $3 $4 >/dev/null 2>&1; true" >/dev/null || true
}
ensure_dnat() { # label  — host APP_PORT -> guest :8000 so you can curl the agent
    sh_host "$1" "
        sudo iptables -t nat -N SW_DNAT 2>/dev/null || true
        sudo iptables -t nat -C PREROUTING -p tcp --dport $APP_PORT -j SW_DNAT 2>/dev/null || sudo iptables -t nat -A PREROUTING -p tcp --dport $APP_PORT -j SW_DNAT
        sudo iptables -t nat -C POSTROUTING -d 10.200.0.0/24 -p tcp --dport 8000 -j MASQUERADE 2>/dev/null || sudo iptables -t nat -A POSTROUTING -d 10.200.0.0/24 -p tcp --dport 8000 -j MASQUERADE
        sudo iptables -t nat -F SW_DNAT; sudo iptables -t nat -A SW_DNAT -p tcp --dport $APP_PORT -j DNAT --to-destination $GUEST_IP:8000
        true" >/dev/null || true
}

if [[ "${1:-}" == "watch" ]]; then watch_logs; exit 0; fi

_log "resetting daemons with agent env (model $MODEL, gap ${GAP}s) + overlay/egress"
reset_agent_daemon a server_a
reset_agent_daemon b server_b
wait_healthz "$A"
wait_healthz "$B"
ensure_net a "10.200.0.1/24" "$A" "$B"
ensure_net b "none" "$B" "$A"
ensure_dnat a
ensure_dnat b
sleep 2

_log "spawning agent VM on A (${MIB}MiB) — guestd waits for the key, then starts aider"
vm=$(curl -s -m60 -X POST "http://$A:8080/vms/spawn?mib=$MIB&net=1" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("vm",""))' 2>/dev/null)
[[ -n "$vm" ]] || _die "spawn failed (check /tmp/hostd.log on A; did the rootfs build + key deliver?)"
_log "vm $vm spawned — waiting for the agent HTTP on http://$A:$APP_PORT"
for _ in $(seq 1 30); do
    [[ -n "$(curl -s -m3 "http://$A:$APP_PORT/" 2>/dev/null)" ]] && { _log "agent ready."; break; }
    sleep 2
done
echo
echo "  terminal 1:  ./scripts/talk-agent.sh           (you drive the agent — one prompt = one turn)"
echo "  terminal 2:  ./scripts/agent-status.sh         (live: which host holds the VM + turn count)"
echo "  terminal 3:  ./scripts/migrate.sh              (move A<->B; lands only between your prompts)"
echo
_log "type a prompt in terminal 1, then migrate in terminal 3 while you're thinking — the agent keeps going on the other host."
