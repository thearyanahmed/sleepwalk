#!/usr/bin/env bash
# Terminal 2 (live): watch the fleet and print only when something changes —
# which host the demo VM is on, each daemon's VM count, and the app's boot_id.
# A migration shows up as a "MIGRATION A->B" line; a changed boot_id (which would
# mean the VM cold-restarted and lost its RAM) shows up as an alarm. The raw
# counter is shown but does not by itself trigger a line (it changes constantly).
# Polls once a second. Ctrl-C to stop.

source "$(dirname "$0")/ensure-env.sh"
# This is a watch loop that must survive the app/VM going unreachable mid-migration
# (curl exits 28). lib.sh sets `set -e`, which would kill the loop on the first
# failed `var=$(curl ...)`; turn it off so the DOWN/`?` fallbacks below can run.
set +e

count_vms() { curl -s -m3 "http://$1:8080/status" 2>/dev/null | python3 -c 'import sys,json;print(len(json.load(sys.stdin)["vms"]))' 2>/dev/null; }
field() { python3 -c "import sys,json;print(json.load(sys.stdin).get('$1',''))" 2>/dev/null; }

echo "live status — prints on change (Ctrl-C to stop)"
prev="" prevloc="" prevboot=""
while true; do
    ts=$(date +%H:%M:%S)
    acount=$(count_vms "$A"); acount=${acount:-?}
    bcount=$(count_vms "$B"); bcount=${bcount:-?}
    if   [[ -n "$(vm_at_guest_ip "http://$A:8080")" ]]; then loc=A
    elif [[ -n "$(vm_at_guest_ip "http://$B:8080")" ]]; then loc=B
    else loc=none; fi
    st=$(curl -s -m3 "http://$A:$APP_PORT/state" 2>/dev/null)
    if [[ -n "$st" ]]; then
        counter=$(echo "$st" | field counter); boot=$(echo "$st" | field boot_id); app=up
    else
        counter="-"; boot="-"; app=DOWN
    fi
    # Trigger on location / boot / vm-counts / app reachability — NOT on counter.
    key="$loc|$boot|$acount|$bcount|$app"
    if [[ "$key" != "$prev" ]]; then
        note=""
        [[ -n "$prevloc" && "$prevloc" != "$loc" && "$loc" != none && "$prevloc" != none ]] && note=" <- MIGRATION $prevloc->$loc"
        [[ -n "$prevboot" && "$prevboot" != "-" && "$boot" != "-" && "$prevboot" != "$boot" ]] && note="$note <- BOOT CHANGED (state lost!)"
        printf '[%s] VM:%s  A:%s B:%s  app:%s counter=%s boot=%s%s\n' \
            "$ts" "$loc" "$acount" "$bcount" "$app" "$counter" "$boot" "$note"
        prev="$key"; prevloc="$loc"; prevboot="$boot"
    fi
    sleep 1
done
