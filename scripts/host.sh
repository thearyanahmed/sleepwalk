#!/usr/bin/env bash
# Resolve a host label into the generic connection vars and hand off to
# remote.sh. This is the ONLY place that knows about hosts A / B — it reads their
# REMOTE_A_* / REMOTE_B_* blocks from the gitignored .env, exports the R* vars
# remote.sh expects, and execs it. remote.sh itself stays host-agnostic.
#
# Usage:
#   scripts/host.sh a sync
#   scripts/host.sh b run migrate-recv 0.0.0.0:9000

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

ENV_FILE="$SLEEPWALK_ROOT/.env"
[[ -f "$ENV_FILE" ]] || _die "no .env — copy .env.example to .env and fill it in"

set -a
# shellcheck disable=SC1090
source "$ENV_FILE"
set +a

label="${1:-}"
[[ -n "$label" ]] || _die "usage: host.sh <a|b> <remote.sh args...>"
shift
case "$label" in
    a | A) prefix="REMOTE_A" ;;
    b | B) prefix="REMOTE_B" ;;
    *) _die "unknown host '$label' (use a or b)" ;;
esac

# Map the selected block (REMOTE_<X>_*) onto the R* vars remote.sh reads.
hv="${prefix}_HOST"
export RHOST="${!hv:?set ${prefix}_HOST in .env}"
uv="${prefix}_USER"
export RUSER="${!uv:-root}"
pv="${prefix}_PORT"
export RPORT="${!pv:-22}"
av="${prefix}_PATH"
export RPATH="${!av:-sleepwalk}"
kv="${prefix}_SSH_KEY"
export RKEY="${!kv:-}"
wv="${prefix}_PASSWORD"
export RPASS="${!wv:-}"

exec "$SLEEPWALK_ROOT/scripts/remote.sh" "$@"
