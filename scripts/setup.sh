#!/usr/bin/env bash
# Provision a Linux host to run Firecracker. Runs INSIDE the guest — the path A'
# dev VM, or a remote box (path B), or native Linux (path C). Idempotent.
#
# The headline check is /dev/kvm: on path A' it only exists because QEMU is
# emulating EL2 in software. If this fails on the dev VM, path A' is dead on this
# machine and tiers 2-3 must go remote (see docs/environment.md).
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
# and a couple of OS tools; tap/NAT networking deps come in Phase 3.
#
# These ship in the Ubuntu cloud image already, so we install ONLY what's missing
# and skip apt entirely otherwise. That matters on path A' (TCG): apt-get triggers
# Ubuntu's update-notifier (`apt-check`), command-not-found (`cnf-update-db`) and
# unattended-upgrades hooks, each of which is single-threaded and pathologically
# slow under emulation — minutes of pegged vCPUs and a stuck dpkg lock.
#   curl/ca-certificates → fetch artifacts   tar → unpack FC   ip → networking
#   mkfs.ext4 (e2fsprogs) → build rootfs
declare -A pkg_for=( [curl]=curl [tar]=tar [ip]=iproute2 [mkfs.ext4]=e2fsprogs )
missing_pkgs=()
for tool in "${!pkg_for[@]}"; do
    command -v "$tool" >/dev/null 2>&1 || missing_pkgs+=("${pkg_for[$tool]}")
done
# ca-certificates has no single binary; presence-check the bundle path.
[[ -e /etc/ssl/certs/ca-certificates.crt ]] || missing_pkgs+=(ca-certificates)

if [[ ${#missing_pkgs[@]} -eq 0 ]]; then
    _log "all runtime deps already present — skipping apt"
else
    _log "installing missing deps: ${missing_pkgs[*]}"
    # Defuse the TCG-hostile apt hooks before touching apt (idempotent; harmless
    # on real hardware). update-notifier + command-not-found are the worst.
    sudo rm -f /etc/apt/apt.conf.d/50command-not-found \
               /etc/apt/apt.conf.d/99update-notifier
    sudo systemctl stop unattended-upgrades 2>/dev/null || true
    sudo apt-get update -qq
    sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
        "${missing_pkgs[@]}" >/dev/null
fi

# ── report ───────────────────────────────────────────────────────────────────
cat <<EOF

$(_log "host ready")
  arch        : $(_arch)
  /dev/kvm    : present
  kvm groups  : $(getent group kvm || echo '(none)')
  next        : fetch artifacts (just fetch) and boot a microVM (just up, Unit 0.4)
EOF
