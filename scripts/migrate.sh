#!/usr/bin/env bash
# Terminal 2: migrate the demo VM to the other host, once.
#
# Auto-detects where the VM is and moves it (A->B, or B->A if it's already on B).
# The two daemon calls it makes:
#   recv (on the TARGET): opens a socket and waits to RECEIVE the snapshot stream;
#        returns once it's listening (the restore runs in the background).
#   send (on the SOURCE): drains the guest to quiescence, snapshots it, and
#        STREAMS it to the target; on success the source drops its copy, so the
#        VM now lives on the target.
# recv first (target ready), then send.

source "$(dirname "$0")/ensure-env.sh"

vm=$(vm_at_guest_ip "http://$A:8080"); src="$A"; dst="$B"; dir="A->B"
if [[ -z "$vm" ]]; then
    vm=$(vm_at_guest_ip "http://$B:8080"); src="$B"; dst="$A"; dir="B->A"
fi
[[ -n "$vm" ]] || _die "no demo VM at $GUEST_IP on A or B — run ./scripts/prepare.sh"

_log "migrating $vm  $dir"
# 1) target opens the receiver and starts listening.
recv=$(curl -s -m10 -X POST "http://$dst:8080/migrate/recv?listen=0.0.0.0:$DATA_PORT")
if [[ "$recv" != "receiving" ]]; then
    _die "receiver on $dst not ready: ${recv:-<no response>}"
fi
# 2) source drains, snapshots, streams. Success returns the timing JSON.
resp=$(curl -s -m90 -X POST "http://$src:8080/migrate/send?vm=$vm&to=$dst:$DATA_PORT")
case "$resp" in
    *snapshot_ms*) echo "$resp" ;;
    *) _die "migration failed: ${resp:-<no response>}" ;;
esac
# 3) verify the VM actually landed on the target.
sleep 2
if [[ -n "$(vm_at_guest_ip "http://$dst:8080")" ]]; then
    _log "done — VM is now on ${dst} (was ${dir%-*}); the same counter continues via http://$A:$APP_PORT"
else
    _warn "send returned ok but the VM was not found on the target — check daemon logs"
fi
_log "run ./scripts/migrate.sh again to move it back, or ./scripts/prepare.sh for a fresh VM"
