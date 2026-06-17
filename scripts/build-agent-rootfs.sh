#!/usr/bin/env bash
# Build the ext4 rootfs for the AGENT profile: a full Ubuntu userland with an
# open-source coding agent (aider) preinstalled, with the static guestd as init
# (PID 1) in wrap mode. guestd supervises the HTTP turn server
# (images/agent/agent-serve.py — one POST /ask = one turn), infers turn boundaries
# from its stdout markers, and — because this profile sets
# /etc/sleepwalk/wrap-await-secrets — defers spawning it until the host hands over
# the model API key via the Secrets vsock message.
#
# No Docker: an ubuntu-base tarball is extracted and provisioned in a chroot, then
# imaged with mkfs.ext4 -d. Output: images/artifacts/agent-rootfs-<arch>.ext4.
#
# Runs on a Linux build host (root, for chroot/mount). Usage:
#   sudo scripts/build-agent-rootfs.sh

set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

[[ "$(_os)" == "Linux" ]] || _die "agent rootfs build runs on Linux (chroot + mkfs.ext4)"
[[ "$(id -u)" -eq 0 ]] || _die "run as root (chroot + bind mounts): sudo scripts/build-agent-rootfs.sh"

ARCH="$(_arch)"                       # x86_64 | aarch64
case "$ARCH" in
    x86_64)  UB_ARCH=amd64 ;;
    aarch64) UB_ARCH=arm64 ;;
    *) _die "unsupported arch $ARCH" ;;
esac
TARGET="${ARCH}-unknown-linux-musl"
UB_REL="24.04"                        # release series (the cdimage directory)
UB_POINT="24.04.4"                     # pinned point release (the tarball name)
UB_TAR="ubuntu-base-${UB_POINT}-base-${UB_ARCH}.tar.gz"
UB_URL="https://cdimage.ubuntu.com/ubuntu-base/releases/${UB_REL}/release/${UB_TAR}"
AIDER_PKG="aider-chat"                # pin (==X.Y.Z) once a known-good version is chosen
OUT="$SLEEPWALK_ROOT/images/artifacts"
CACHE="$OUT/cache"
IMG="$OUT/agent-rootfs-${ARCH}.ext4"
mkdir -p "$OUT" "$CACHE"

_need cargo "install the Rust toolchain (scripts/setup.sh)"
_need mkfs.ext4 "apt install e2fsprogs"
_need curl "apt install curl"

_log "building static guestd for $TARGET (the rootfs init)"
rustup target add "$TARGET" >/dev/null 2>&1 || true
( cd "$SLEEPWALK_ROOT" && cargo build -q -p guestd --bin guestd --release --target "$TARGET" )
BIN="$SLEEPWALK_ROOT/target/$TARGET/release/guestd"
[[ -x "$BIN" ]] || _die "guestd binary not produced at $BIN"
if command -v file >/dev/null 2>&1 && file "$BIN" | grep -q "dynamically linked"; then
    _die "guestd is dynamically linked; musl static build expected"
fi

if [[ ! -f "$CACHE/$UB_TAR" ]]; then
    _log "fetching $UB_TAR"
    curl -fSL "$UB_URL" -o "$CACHE/$UB_TAR"
fi

ROOT="$(mktemp -d)"
unmount_binds() {
    for m in dev/pts dev proc sys; do
        mountpoint -q "$ROOT/$m" && umount -lf "$ROOT/$m" || true
    done
}
# On any error, unmount then drop the tree. The happy path unmounts explicitly
# BEFORE mkfs (so the image excludes /proc,/sys) and removes the tree after.
trap 'unmount_binds; rm -rf "$ROOT"' EXIT

_log "extracting ubuntu-base into rootfs tree"
tar -xzf "$CACHE/$UB_TAR" -C "$ROOT"

# Network + mounts so apt/pip work inside the chroot.
cp /etc/resolv.conf "$ROOT/etc/resolv.conf"
mount -t proc proc "$ROOT/proc"
mount -t sysfs sys "$ROOT/sys"
mount -o bind /dev "$ROOT/dev"
mount -o bind /dev/pts "$ROOT/dev/pts"

_log "provisioning userland + aider (this pulls a few hundred MB; be patient)"
chroot "$ROOT" /bin/bash -euo pipefail <<CHROOT
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y --no-install-recommends python3 python3-pip git ca-certificates >/dev/null
# Noble enforces PEP-668; this image is single-purpose so a global install is fine.
pip install --break-system-packages --no-cache-dir $AIDER_PKG pytest >/dev/null
# aider needs a git identity even with --no-auto-commits.
git config --global user.email agent@sleepwalk.local
git config --global user.name "sleepwalk agent"
git config --global --add safe.directory /root/task
apt-get clean
rm -rf /var/lib/apt/lists/*
CHROOT

_log "installing init, driver, wrap config, and seed repo"
cp "$BIN" "$ROOT/init"            # kernel execs /init as PID 1 (init=/init)
chmod +x "$ROOT/init"
mkdir -p "$ROOT/etc/sleepwalk" "$ROOT/usr/local/bin" "$ROOT/root/task"
cp "$SLEEPWALK_ROOT/images/agent/agent-serve.py" "$ROOT/usr/local/bin/agent-serve.py"
chmod +x "$ROOT/usr/local/bin/agent-serve.py"
cp "$SLEEPWALK_ROOT/images/agent/seed/"* "$ROOT/root/task/"
# guestd (init) execs this; argv is split on whitespace. stdbuf forces line-
# buffered stdout so the turn markers reach guestd promptly (a pipe is fully
# buffered otherwise, and guestd would never see the boundaries). The agent serves
# HTTP on :8000 — one POST /ask = one turn — so turns are driven on demand.
printf 'stdbuf -oL -eL python3 /usr/local/bin/agent-serve.py\n' > "$ROOT/etc/sleepwalk/wrap-cmd"
# Defer the child until Secrets arrive (the agent needs the API key at exec).
: > "$ROOT/etc/sleepwalk/wrap-await-secrets"
# Seed the git repo so aider has a working tree from the first turn.
chroot "$ROOT" /bin/bash -c 'cd /root/task && git init -q && git add -A && git commit -qm seed'

# The guest has no resolver manager (guestd is PID 1, not systemd) and the kernel
# ip= cmdline sets no DNS, so bake a public resolver. Egress itself is handled by
# net-host.sh (MASQUERADE out the uplink); this just makes names resolve.
printf 'nameserver 8.8.8.8\nnameserver 1.1.1.1\n' > "$ROOT/etc/resolv.conf"

unmount_binds                         # image must not capture /proc,/sys,/dev
trap - EXIT

_log "building ext4 image $IMG (3G)"
rm -f "$IMG"
mkfs.ext4 -F -q -L agentrootfs -d "$ROOT" "$IMG" 3G
rm -rf "$ROOT"

_log "done: $IMG ($(du -h "$IMG" | cut -f1))"
_log "boot it by pointing the daemon at it:  SLEEPWALK_ROOTFS=$IMG"
