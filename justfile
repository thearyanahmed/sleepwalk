# sleepwalk — `just` is the single entry point for every action.
# Targets are grouped by the dev tier they require (see docs/environment.md):
#   tier 1  host-agnostic   — runs anywhere, incl. macOS native (no VM)
#   tier 2  functional KVM  — needs /dev/kvm (path A, A', B, or C)
#   tier 3  real KVM only   — benchmark-valid hosts (path A, B, C — never A'/TCG)
#
# Phase 0 status: most targets are stubs that fail loud until their phase lands.

# Show the target map (default).
default:
    @just --list

# ── tier 1 · host-agnostic ────────────────────────────────────────────────

# Unit + mock tests. Runs on any machine including macOS native.
test:
    cargo test --workspace

# Lint + format gate. Must be clean before any PR is ready.
lint:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings

fmt:
    cargo fmt

# Turn-vs-drain race-rule chaos over seeded interleavings. Deterministic, no VM —
# the fast falsification layer for the race rule; the wall-clock real-VM run is
# `chaos-vm` (tier 2). A failure prints the seed that reproduces it.
chaos:
    cargo test -p harness 'chaos::' -- --nocapture

# ── Phase 0 · environment & artifacts ─────────────────────────────────────

# Download + checksum pinned Firecracker binary and kernel (Unit 0.2).
fetch:
    scripts/fetch-artifacts.sh

# Boot the M1/M2 TCG dev VM with a functional /dev/kvm (Unit 0.3, path A').
dev-vm:
    scripts/dev-vm.sh

# ssh into the running dev VM.
dev-vm-ssh:
    scripts/dev-vm.sh ssh

# Sync the repo into the running dev VM, then provision it (verify /dev/kvm, deps).
dev-vm-setup:
    rsync -az --delete -e 'ssh -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -i images/dev-vm/dev-vm-key' \
        --exclude target --exclude .git --exclude images/dev-vm \
        ./ sleepwalk@localhost:sleepwalk/
    scripts/dev-vm.sh ssh 'cd sleepwalk && scripts/setup.sh'

# Boot one Firecracker microVM by hand (Unit 0.4). Phase 0 exit criterion.
up:
    @echo "not implemented until Phase 0 / Unit 0.4 (first-microvm)" && exit 1

# ── tier 2 · functional KVM (needs /dev/kvm) ──────────────────────────────

# Single-host snapshot/restore lifecycle (Phase 1).
lifecycle-test:
    @echo "not implemented until Phase 1" && exit 1

# Two-host A->B migration (Phase 3).
migrate-test:
    @echo "not implemented until Phase 3" && exit 1

# Turn-vs-drain chaos against real VMs, 100 wall-clock runs (Phase 4).
chaos-vm:
    @echo "not implemented until Phase 4 (real-VM tier)" && exit 1

# ── tier 3 · real KVM only (benchmark-valid — refuses TCG) ─────────────────

# O2 freeze-window table vs RAM size (Phase 2).
bench-restore:
    @echo "not implemented until Phase 2" && exit 1

# Full fleet scenario, O5 (Phase 5).
e2e:
    @echo "not implemented until Phase 5" && exit 1

# Agent demo: a coding agent survives migration mid-session, O6 (Phase 5b).
demo-agent:
    @echo "not implemented until Phase 5b" && exit 1

# Prometheus + Grafana stack (Phase 5).
observe:
    @echo "not implemented until Phase 5" && exit 1
