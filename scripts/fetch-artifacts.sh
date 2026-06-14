#!/usr/bin/env bash
# Download + checksum the pinned Firecracker binary and guest kernel for this
# host's arch, per images/versions.toml. Self-pins unpinned hashes (prints the
# computed value to paste back). Idempotent: re-verifies what's already present.
#
# Usage: scripts/fetch-artifacts.sh            (`just fetch`)

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

_need curl "install curl"

ARCH="$(_arch)"
VERSIONS="$SLEEPWALK_ROOT/images/versions.toml"
OUT="$SLEEPWALK_ROOT/images/artifacts"
mkdir -p "$OUT"

[[ -f "$VERSIONS" ]] || _die "missing $VERSIONS"

unpinned=0

# Download $url to $dest (skip if present), then verify against $expected_hash.
# Records unpinned artifacts so we can fail with a single actionable summary.
fetch_one() {
    local name="$1" url="$2" dest="$3" expected="$4"
    [[ -n "$url" ]] || _die "no URL pinned for $name ($ARCH) in versions.toml — verify + pin first"
    if [[ -f "$dest" ]]; then
        _log "$name already present, verifying"
    else
        _log "downloading $name <- $url"
        curl -fSL --retry 3 -o "$dest.partial" "$url" || _die "download failed: $url"
        mv "$dest.partial" "$dest"
    fi
    if ! _verify_sha256 "$dest" "$expected"; then
        unpinned=1
    fi
}

# ── Firecracker binary ───────────────────────────────────────────────────────
fc_version="$(_toml_get "$VERSIONS" firecracker version)"
[[ -n "$fc_version" ]] || _die "firecracker.version is empty in versions.toml — verify against GitHub releases and pin it"
fc_hash="$(_toml_get "$VERSIONS" firecracker "sha256_$ARCH")"
# Canonical release URL layout: firecracker-<ver>-<arch>.tgz from the GitHub release.
fc_url="https://github.com/firecracker-microvm/firecracker/releases/download/${fc_version}/firecracker-${fc_version}-${ARCH}.tgz"
fetch_one "firecracker ${fc_version}" "$fc_url" "$OUT/firecracker-${fc_version}-${ARCH}.tgz" "$fc_hash"

# ── guest kernel ─────────────────────────────────────────────────────────────
k_version="$(_toml_get "$VERSIONS" kernel version)"
k_url="$(_toml_get "$VERSIONS" kernel "url_$ARCH")"
k_hash="$(_toml_get "$VERSIONS" kernel "sha256_$ARCH")"
[[ -n "$k_version" ]] || _die "kernel.version is empty in versions.toml — pin a specific CI kernel build"
fetch_one "kernel ${k_version}" "$k_url" "$OUT/vmlinux-${k_version}-${ARCH}" "$k_hash"

# ── summary ──────────────────────────────────────────────────────────────────
if [[ "$unpinned" -eq 1 ]]; then
    cat >&2 <<EOF

$(_warn "one or more artifacts are UNPINNED (sha256 empty in versions.toml).")
  Paste the computed sha256 values printed above into images/versions.toml,
  then re-run \`just fetch\` to confirm a clean verified state.
EOF
    exit 9
fi

_log "all artifacts present and verified for $ARCH"
