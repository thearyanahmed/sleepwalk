#!/usr/bin/env bash
# Shared helpers for sleepwalk host-side scripts (run on the macOS/Linux host,
# NOT inside the guest). Source this; do not execute it directly.
#
#   source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

set -euo pipefail

# Repo root, resolved regardless of where the caller is invoked from.
SLEEPWALK_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
export SLEEPWALK_ROOT

# logging
_log()  { printf '\033[1;34m[sleepwalk]\033[0m %s\n' "$*" >&2; }
_warn() { printf '\033[1;33m[sleepwalk] warn:\033[0m %s\n' "$*" >&2; }
_die()  { printf '\033[1;31m[sleepwalk] error:\033[0m %s\n' "$*" >&2; exit 1; }

# platform
_os()   { uname -s; }   # Darwin | Linux
_arch() {               # normalise to Firecracker/kernel naming
    case "$(uname -m)" in
        arm64|aarch64) echo "aarch64" ;;
        x86_64|amd64)  echo "x86_64"  ;;
        *) _die "unsupported arch: $(uname -m)" ;;
    esac
}

# Require a command on PATH or die with an install hint.
_need() {
    local cmd="$1" hint="${2:-}"
    command -v "$cmd" >/dev/null 2>&1 && return 0
    _die "missing required tool: $cmd${hint:+ — $hint}"
}

# checksums (portable across macOS `shasum` and Linux `sha256sum`)
_sha256() {
    local f="$1"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$f" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$f" | awk '{print $1}'
    else
        _die "no sha256 tool (need sha256sum or shasum)"
    fi
}

# Verify $1 against expected hex $2. Empty expected => print computed + signal
# "unpinned" via return code 9 so callers can offer to self-pin.
_verify_sha256() {
    local f="$1" expected="${2:-}" got
    got="$(_sha256 "$f")"
    if [[ -z "$expected" ]]; then
        _warn "unpinned: $(basename "$f") sha256=$got"
        return 9
    fi
    [[ "$got" == "$expected" ]] || _die "sha256 mismatch for $(basename "$f")
  expected $expected
  got      $got"
    _log "verified $(basename "$f")"
}

# minimal TOML reader
# Reads `key = "value"` (or bare value) under a `[section]` from a flat TOML
# file. Good enough for images/versions.toml; not a general TOML parser.
_toml_get() {
    local file="$1" section="$2" key="$3"
    awk -v sec="$section" -v k="$key" '
        /^\[/ { in_sec = ($0 == "[" sec "]") }
        in_sec && $1 == k {
            sub(/^[^=]*=[ \t]*/, "")   # drop "key = "
            sub(/[ \t]*#.*$/, "")      # drop trailing comment (values hold no #)
            sub(/[ \t]+$/, "")         # trim trailing ws
            sub(/^"/, ""); sub(/"$/, "")   # strip one surrounding quote each
            print; exit
        }
    ' "$file"
}
