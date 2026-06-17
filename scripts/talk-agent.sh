#!/usr/bin/env bash
# Talk to the agent yourself. Each line you type is ONE turn: it POSTs to the agent
# (POST /ask) which runs aider once. While you read the reply and think of the next
# prompt, the VM is idle — that idle window is exactly when ./scripts/migrate.sh can
# move it between hosts. The reply comes back from whichever host currently holds the
# VM (the overlay + DNAT follow it), so you can migrate mid-conversation and just
# keep typing.
source "$(dirname "$0")/ensure-env.sh"

URL="http://$A:$APP_PORT/ask"
_log "talking to the agent at $URL  —  one prompt = one turn  (Ctrl-D to quit)"
echo "    e.g.  Add a power(base, exp) function to calc.py"
echo "    e.g.  Add tests for power() to test_calc.py"

while true; do
    printf '\n\033[1;36myou>\033[0m '
    IFS= read -r prompt || { echo; break; }
    [[ -z "$prompt" ]] && continue
    printf '[%s] sending (this turn blocks a migration until it finishes)…\n' "$(date +%H:%M:%S)"
    reply=$(curl -s -m180 -X POST "$URL" --data "$prompt" 2>/dev/null)
    if [[ -z "$reply" ]]; then
        echo "(no response — VM may be mid-move; wait a moment and retry)"
        continue
    fi
    printf '%s' "$reply" | python3 -c '
import sys, json
raw = sys.stdin.read()
try:
    d = json.loads(raw)
    print("\033[1;32magent> [turn %s]\033[0m" % d.get("turn"))
    print((d.get("reply") or d.get("error") or "").strip())
except Exception:
    print(raw)
' 2>/dev/null || printf '%s\n' "$reply"
done
