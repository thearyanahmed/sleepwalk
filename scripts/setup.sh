#!/usr/bin/env bash
# Provision a Linux host to run Firecracker. Runs INSIDE the guest — the path A'
# dev VM, or a remote box (path B), or native Linux (path C). Idempotent.
#
# The headline check is /dev/kvm: on path A' it only exists because QEMU is
# emulating EL2 in software. If this fails on the dev VM, path A' is dead on this
# machine and tiers 2-3 must go remote (project-plan §2).
#
# Usage (in guest): scripts/setup.sh

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

[[ "$(_os)" == "Linux" ]] || _die "setup.sh runs inside the Linux guest/host, not on $(_os)"

# ── the gate: /dev/kvm ───────────────────────────────────────────────────────
_log "checking /dev/kvm"
[[ -e /dev/kvm ]] || _die "/dev/kvm absent — no KVM here.
  On the path A' dev VM this means QEMU is not exposing emulated EL2; confirm
  the VM was booted with -machine virt,virtualization=on -cpu max. On bare metal,
  enable virtualization / load the kvm module."

if [[ -r /dev/kvm && -w /dev/kvm ]]; then
    _log "/dev/kvm present and accessible"
else
    _warn "/dev/kvm present but not r/w for $(whoami); adding to kvm group (re-login needed)"
    sudo usermod -aG kvm "$(whoami)" || _die "could not add $(whoami) to kvm group"
fi

# ── runtime deps ─────────────────────────────────────────────────────────────
# Firecracker ships as a static binary, so no build toolchain — just fetch/unpack
# tools now; tap/NAT networking deps come in Phase 3.
_log "installing runtime deps"
sudo apt-get update -qq
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
    curl ca-certificates tar e2fsprogs iproute2 >/dev/null

# ── report ───────────────────────────────────────────────────────────────────
cat <<EOF

$(_log "host ready")
  arch        : $(_arch)
  /dev/kvm    : present
  kvm groups  : $(getent group kvm || echo '(none)')
  next        : fetch artifacts (just fetch) and boot a microVM (just up, Unit 0.4)
EOF
