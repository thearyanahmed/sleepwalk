#!/usr/bin/env bash
# Boot the M1/M2 dev VM (path A'): an aarch64 Ubuntu guest under QEMU TCG with
# software-emulated EL2, so a *functional* /dev/kvm exists inside it and
# Firecracker can boot microVMs. ~10-30x slower than native — dev/correctness
# only, NEVER benchmark-valid (see docs/environment.md).
#
# Usage:
#   scripts/dev-vm.sh            boot (foreground, serial console)
#   scripts/dev-vm.sh ssh        ssh into a running dev VM
# After boot, ssh is on localhost:2222 (also: `just dev-vm-ssh`).

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

ARCH="$(_arch)"
[[ "$ARCH" == "aarch64" ]] || _die "dev-vm (path A') is aarch64-only; on x86_64 use path C (native KVM) directly"

WORK="$SLEEPWALK_ROOT/images/dev-vm"
mkdir -p "$WORK"
BASE_IMG="$WORK/ubuntu-base.qcow2"      # immutable downloaded cloud image
OVERLAY="$WORK/dev-vm.qcow2"            # writable overlay; delete to reset
SEED="$WORK/seed.iso"                   # cloud-init NoCloud
VARS="$WORK/edk2-vars.fd"               # writable UEFI vars
SSH_PORT=2222

EDK2_CODE="$(brew --prefix qemu 2>/dev/null)/share/qemu/edk2-aarch64-code.fd"
[[ -f "$EDK2_CODE" ]] || EDK2_CODE="/usr/share/qemu/edk2-aarch64-code.fd"

# subcommand: ssh
if [[ "${1:-}" == "ssh" ]]; then
    shift
    exec ssh -p "$SSH_PORT" \
        -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        -i "$WORK/dev-vm-key" sleepwalk@localhost "$@"
fi

_need qemu-system-aarch64 "brew install qemu"
_need qemu-img "brew install qemu"
_need ssh-keygen

# ssh key (guest gets the pubkey via cloud-init)
if [[ ! -f "$WORK/dev-vm-key" ]]; then
    _log "generating dev-vm ssh key"
    ssh-keygen -t ed25519 -N "" -f "$WORK/dev-vm-key" -C "sleepwalk-dev-vm" >/dev/null
fi
PUBKEY="$(cat "$WORK/dev-vm-key.pub")"

# base cloud image
if [[ ! -f "$BASE_IMG" ]]; then
    img_url="$(_toml_get "$SLEEPWALK_ROOT/images/versions.toml" dev_vm ubuntu_img_aarch64)"
    [[ -n "$img_url" ]] || _die "dev_vm.ubuntu_img_aarch64 not pinned in versions.toml — set the cloud image URL"
    img_hash="$(_toml_get "$SLEEPWALK_ROOT/images/versions.toml" dev_vm sha256_aarch64)"
    _log "downloading Ubuntu cloud image"
    curl -fSL --retry 3 -o "$BASE_IMG.partial" "$img_url" || _die "image download failed"
    _verify_sha256 "$BASE_IMG.partial" "$img_hash" || _die "dev_vm.sha256_aarch64 empty — pin it from the release SHA256SUMS"
    mv "$BASE_IMG.partial" "$BASE_IMG"
fi

# writable overlay (reset = delete this)
if [[ ! -f "$OVERLAY" ]]; then
    _log "creating overlay disk (40G sparse)"
    qemu-img create -f qcow2 -F qcow2 -b "$BASE_IMG" "$OVERLAY" 40G >/dev/null
fi

# cloud-init seed (NoCloud)
if [[ ! -f "$SEED" ]]; then
    _log "building cloud-init seed"
    tmp="$(mktemp -d)"
    cat > "$tmp/meta-data" <<EOF
instance-id: sleepwalk-dev-vm
local-hostname: sleepwalk-dev
EOF
    sed "s|__PUBKEY__|$PUBKEY|" "$SLEEPWALK_ROOT/scripts/cloud-init/user-data.yaml" > "$tmp/user-data"
    # NoCloud datasource keys on a filesystem labelled "cidata".
    if [[ "$(_os)" == "Darwin" ]]; then
        hdiutil makehybrid -o "$SEED" -iso -joliet -default-volume-name CIDATA "$tmp" >/dev/null
    elif command -v cloud-localds >/dev/null 2>&1; then
        cloud-localds "$SEED" "$tmp/user-data" "$tmp/meta-data"
    else
        _need genisoimage "install genisoimage/xorriso"
        genisoimage -output "$SEED" -volid CIDATA -joliet -rock "$tmp/user-data" "$tmp/meta-data"
    fi
    rm -rf "$tmp"
fi

# writable UEFI vars
[[ -f "$EDK2_CODE" ]] || _die "edk2 UEFI firmware not found ($EDK2_CODE) — is qemu installed?"
if [[ ! -f "$VARS" ]]; then
    # virt pflash banks are 64MiB each; vars must match the code bank size.
    dd if=/dev/zero of="$VARS" bs=1m count=64 2>/dev/null
fi

# launch
_log "booting dev VM under TCG (emulated EL2) — this is SLOW (minutes), be patient"
_log "ssh: scripts/dev-vm.sh ssh   (or: just dev-vm-ssh)   port $SSH_PORT"
exec qemu-system-aarch64 \
    -name sleepwalk-dev-vm \
    -machine virt,virtualization=on,gic-version=3 \
    -cpu max \
    -accel tcg,thread=multi \
    -smp 4 \
    -m 6144 \
    -drive "if=pflash,format=raw,readonly=on,file=$EDK2_CODE" \
    -drive "if=pflash,format=raw,file=$VARS" \
    -drive "if=virtio,format=qcow2,file=$OVERLAY" \
    -drive "if=virtio,format=raw,file=$SEED" \
    -netdev "user,id=net0,hostfwd=tcp::${SSH_PORT}-:22" \
    -device virtio-net-pci,netdev=net0 \
    -nographic
