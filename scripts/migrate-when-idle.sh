#!/usr/bin/env bash
# Auto-migrate at the next idle gap. Waits until the agent is between turns, then
# calls ./scripts/migrate.sh to move it A<->B. No manual timing.
#
# How "busy" is detected without disturbing anything: the agent's HTTP server is
# single-threaded, so while it is running a turn (a /ask) it cannot answer a probe.
# A fast GET that succeeds => idle; one that times out => mid-turn. We only attempt
# the migration when idle, so a stood-down receiver never orphans a port.
source "$(dirname "$0")/ensure-env.sh"

MAX="${MIGRATE_MAX_TRIES:-60}"
PROBE="http://$A:$APP_PORT/"
MIGRATE="$(dirname "$0")/migrate.sh"

_log "waiting for an idle gap, then migrating (Ctrl-C to stop)…"
for i in $(seq 1 "$MAX"); do
    if ! curl -s -m2 "$PROBE" >/dev/null 2>&1; then
        _log "agent mid-turn — waiting for it to finish… ($i/$MAX)"
        sleep 3
        continue
    fi
    # Idle: take the gap.
    out=$("$MIGRATE" 2>&1)
    echo "$out" | grep -avE 'gsk_'
    if echo "$out" | grep -qiE "done|now on|complete"; then
        exit 0
    elif echo "$out" | grep -qi "address already in use"; then
        _log "a previous receiver is still releasing its port — waiting… ($i/$MAX)"
        sleep 8
    else
        # A turn started in the split second after our probe; wait and retry.
        _log "raced a turn start — retrying… ($i/$MAX)"
        sleep 3
    fi
done
_die "no idle gap caught in $MAX tries"
