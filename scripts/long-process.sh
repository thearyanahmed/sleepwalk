#!/usr/bin/env bash
# Terminal 1: the long-running client. Hammers the app's POST /inc in a loop and
# prints the in-RAM counter. Leave it running and migrate in another terminal —
# the counter keeps climbing with the same boot_id across the move (a brief blip
# during the freeze). Always targets host A's public port; the DNAT + overlay
# route it to the VM wherever it currently lives. Ctrl-C to stop.

source "$(dirname "$0")/ensure-env.sh"

echo "hammering http://$A:$APP_PORT/inc — counter from VM RAM (Ctrl-C to stop)"
while true; do
    curl -s -m2 -X POST "http://$A:$APP_PORT/inc" || printf '(no response — migrating?)'
    echo
    sleep 0.3
done
