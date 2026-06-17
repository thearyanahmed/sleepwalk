#!/usr/bin/env bash
# Terminal 1, variant: a *bursty* client that mimics a real workload's turn/idle
# rhythm instead of hammering flat-out. Each cycle it runs one "turn" — a
# POST /busy?secs=N that stalls the single-threaded server for N seconds (the VM
# is busy, an idle-probe times out) and bumps the counter — then sits idle for a
# random 3-10s pause. That pause is the quiescence window: `migrate-when-idle`
# probes the server, sees the turn as busy, and only moves the VM during a gap.
# The counter keeps climbing with the same boot_id across the move; no blip
# mid-turn. Always targets host A's public port; DNAT + overlay route it to the
# VM wherever it lives. Ctrl-C to stop.
source "$(dirname "$0")/ensure-env.sh"
set +e  # tolerate the app going unreachable mid-migration (curl exits 28)

URL="http://$A:$APP_PORT/busy"

echo "bursty load on $URL — turns + random idle gaps (Ctrl-C to stop)"
while true; do
    busy=$((RANDOM % 4 + 3))   # 3-6s turn
    printf '\033[1;33m[%s] ▶ turn: busy %ds…\033[0m\n' "$(date +%H:%M:%S)" "$busy"
    resp=$(curl -s -m"$((busy + 5))" -X POST "$URL?secs=$busy") || resp='(no response — migrating?)'
    printf '  %s\n' "$resp"
    gap=$((RANDOM % 8 + 3))    # 3-10s idle
    printf '\033[1;32m[%s] ⏸ idle %ds — migrate-when-idle moves here\033[0m\n' "$(date +%H:%M:%S)" "$gap"
    sleep "$gap"
done
