#!/usr/bin/env bash
# Build a minimal ext4 rootfs whose init is a static guestd, for the synthetic
# profile. guestd is linked against musl so the rootfs needs no shared libraries;
# the image is populated with mkfs.ext4 -d (no loop mount, no root beyond what
# mkfs needs). Output: images/artifacts/guestd-rootfs-<arch>.ext4.
#
# Runs on a Linux build host. Usage: scripts/build-guest-rootfs.sh

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

[[ "$(_os)" == "Linux" ]] || _die "rootfs build runs on Linux (musl static + mkfs.ext4)"
ARCH="$(_arch)"
TARGET="${ARCH}-unknown-linux-musl"
OUT="$SLEEPWALK_ROOT/images/artifacts"
mkdir -p "$OUT"

_need cargo "install the Rust toolchain (scripts/setup.sh)"
_need mkfs.ext4 "apt install e2fsprogs"

_log "adding rust target $TARGET"
rustup target add "$TARGET" >/dev/null 2>&1 || true

_log "building static guestd for $TARGET"
( cd "$SLEEPWALK_ROOT" && cargo build -q -p guestd --bin guestd --release --target "$TARGET" )
BIN="$SLEEPWALK_ROOT/target/$TARGET/release/guestd"
[[ -x "$BIN" ]] || _die "guestd binary not produced at $BIN"
# Confirm it is a static binary (no interpreter) — a dynamic one won't run in the
# bare rootfs.
if command -v file >/dev/null 2>&1 && file "$BIN" | grep -q "dynamically linked"; then
    _die "guestd is dynamically linked; musl static build expected"
fi

_log "building static ramstate workload for $TARGET"
( cd "$SLEEPWALK_ROOT" && cargo build -q -p ramstate --bin ramstate --release --target "$TARGET" )
APP="$SLEEPWALK_ROOT/target/$TARGET/release/ramstate"
[[ -x "$APP" ]] || _die "ramstate binary not produced at $APP"
if command -v file >/dev/null 2>&1 && file "$APP" | grep -q "dynamically linked"; then
    _die "ramstate is dynamically linked; musl static build expected"
fi

_log "assembling rootfs tree"
ROOT="$(mktemp -d)"
trap 'rm -rf "$ROOT"' EXIT
mkdir -p "$ROOT/dev" "$ROOT/proc" "$ROOT/sys" "$ROOT/etc/sleepwalk"
cp "$BIN" "$ROOT/init" # the kernel execs /init as PID 1 (boot arg init=/init)
chmod +x "$ROOT/init"
cp "$APP" "$ROOT/app" # the in-RAM stateful workload guestd supervises (wrap mode)
chmod +x "$ROOT/app"
# guestd (init) reads this and runs /app as its wrapped child; the rootfs has no
# shell, so this file — not an env export — is how wrap mode is selected.
printf '/app\n' > "$ROOT/etc/sleepwalk/wrap-cmd"

IMG="$OUT/guestd-rootfs-${ARCH}.ext4"
rm -f "$IMG"
_log "building ext4 image $IMG"
mkfs.ext4 -F -q -L guestrootfs -d "$ROOT" "$IMG" 64M

_log "done: $IMG ($(du -h "$IMG" | cut -f1))"
