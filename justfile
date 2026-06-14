# sleepwalk — `just` is the single entry point for every action.
# Targets are grouped by the dev tier they require (see docs/environment.md):
#   tier 1  host-agnostic   — runs anywhere, incl. macOS native (no VM)
#   tier 2  functional KVM  — needs /dev/kvm (path A, A', B, or C)
#   tier 3  real KVM only   — benchmark-valid hosts (path A, B, C — never A'/TCG)
#
# Many targets are stubs that fail loud until their capability lands.

# Show the target map (default).
default:
    @just --list

# tier 1 · host-agnostic

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

# environment & artifacts

# Download + checksum pinned Firecracker binary and kernel.
fetch:
    scripts/fetch-artifacts.sh

# Boot the M1/M2 TCG dev VM with a functional /dev/kvm (path A').
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

# Boot one Firecracker microVM by hand — the environment exit criterion.
up:
    @echo "not implemented yet (first-microvm: boot a VM by hand)" && exit 1

# remote · drive a Linux box over SSH (config in gitignored .env)

# Sync the working tree to the remote (copy .env.example -> .env first).
remote-sync:
    scripts/remote.sh sync

# Shell into the remote, or run a one-off command: `just remote-ssh 'nproc'`.
remote-ssh *CMD:
    scripts/remote.sh ssh {{CMD}}

# Sync, then provision the remote (scripts/setup.sh).
remote-setup *ARGS:
    scripts/remote.sh setup {{ARGS}}

# Sync, then run a just target on the remote: `just remote-run test`.
remote-run TARGET:
    scripts/remote.sh run {{TARGET}}

# tier 2 · functional KVM (needs /dev/kvm)

# Single-host snapshot/restore lifecycle.
lifecycle-test:
    @echo "not implemented yet (single-host lifecycle)" && exit 1

# Two-host A->B migration.
migrate-test:
    @echo "not implemented yet (two-host migration)" && exit 1

# Turn-vs-drain chaos against real VMs, 100 wall-clock runs.
chaos-vm:
    @echo "not implemented yet (needs the real-VM tier)" && exit 1

# tier 3 · real KVM only (benchmark-valid — refuses TCG)

# O2 freeze-window table vs RAM size.
bench-restore:
    @echo "not implemented yet (UFFD restore benchmark)" && exit 1

# Full fleet scenario, O5.
e2e:
    @echo "not implemented yet (fleet scenario)" && exit 1

# Agent demo: a coding agent survives migration mid-session, O6.
demo-agent:
    @echo "not implemented yet (agent demo)" && exit 1

# Prometheus + Grafana stack.
observe:
    @echo "not implemented yet (observability stack)" && exit 1
