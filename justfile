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

# Build the mdbook documentation site into docs/book (needs `cargo install mdbook`).
book:
    cd docs && mdbook build

# Serve the docs locally with live reload at http://localhost:3000.
book-serve:
    cd docs && mdbook serve --open

# Turn-vs-drain race-rule chaos over seeded interleavings. Deterministic, no VM —
# the fast falsification layer for the race rule; the wall-clock KVM run is
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

# remote · drive host A over SSH (config in gitignored .env). For host B use
# `scripts/host.sh b <cmd>`; remote.sh itself is host-agnostic.

# Sync the working tree to host A (copy .env.example -> .env first).
remote-sync:
    scripts/host.sh a sync

# Shell into host A, or run a one-off command: `just remote-ssh 'nproc'`.
remote-ssh *CMD:
    scripts/host.sh a ssh {{CMD}}

# Sync, then provision host A (scripts/setup.sh).
remote-setup *ARGS:
    scripts/host.sh a setup {{ARGS}}

# Sync, then run a just target on host A: `just remote-run test`.
remote-run TARGET:
    scripts/host.sh a run {{TARGET}}

# Run the per-host daemon (serves /healthz + /metrics). Runs anywhere.
hostd-daemon ADDR="0.0.0.0:8080":
    cargo run -q -p hostd --bin hostd -- daemon {{ADDR}}

# Build the synthetic guest rootfs (static guestd as init). Linux build host.
guest-rootfs:
    scripts/build-guest-rootfs.sh

# Build the agent guest rootfs (Ubuntu + aider, guestd as init). Linux, root.
agent-rootfs:
    sudo scripts/build-agent-rootfs.sh

# UFFD lazy-restore page server. Needs Linux (userfaultfd) but NOT KVM, so it
# runs on any Linux box: `just remote-run uffd-test`.
uffd-test:
    cargo test -p hostd 'uffd::' -- --nocapture

# Real AF_VSOCK transport round-trip over loopback. Needs Linux + the
# vsock_loopback module (no KVM): `just remote-run vsock-test`.
vsock-test:
    modprobe vsock_loopback 2>/dev/null || true
    cargo test -p guestd --features vsock-test 'vsock::' -- --nocapture

# tier 2 · functional KVM (needs /dev/kvm)

# Single-host Firecracker boot lifecycle (needs /dev/kvm + `just fetch`).
lifecycle-test:
    cargo test -p hostd --features kvm --test lifecycle -- --nocapture

# Single-host snapshot -> UFFD lazy restore of a live VM (needs /dev/kvm + fetch).
restore-test:
    cargo test -p hostd --features kvm --test restore -- --nocapture

# Migration freeze-window benchmark: boot once, ping-pong N migrations, record
# each timing + min/max/mean as JSON. Tunable via SLEEPWALK_BENCH_CYCLES /
# SLEEPWALK_BENCH_SETTLE_MS. Needs /dev/kvm + `just fetch`.
migrate-bench:
    cargo run -q -p hostd --features kvm --bin migrate-bench

# Two-process A->B migration. Start the receiver on the target first, then the
# sender on the source. Loopback: `migrate-recv 127.0.0.1:9000` + `migrate-send
# 127.0.0.1:9000`. Cross-host: run recv on B, point send at B's IP. Needs KVM.
migrate-recv ADDR COUNT="1":
    cargo run -q -p hostd --features kvm --bin migrate -- recv {{ADDR}} {{COUNT}}

migrate-send ADDR COUNT="1":
    cargo run -q -p hostd --features kvm --bin migrate -- send {{ADDR}} {{COUNT}}

# A->B migration with memory streamed over TCP (needs /dev/kvm + `just fetch`).
# Loopback here; point the sender at another droplet's IP for a cross-host run.
migrate-test:
    cargo test -p hostd --features kvm --test migrate -- --nocapture

# Turn-vs-drain chaos against live VMs, 100 wall-clock runs.
chaos-vm:
    @echo "not implemented yet (needs the KVM tier)" && exit 1

# tier 3 · real KVM only (benchmark-valid — refuses TCG)

# O2 freeze-window table vs RAM size.
bench-restore:
    @echo "not implemented yet (UFFD restore benchmark)" && exit 1

# Full fleet scenario, O5.
e2e:
    @echo "not implemented yet (fleet scenario)" && exit 1

# Start a coding agent in a VM (it survives migration mid-session, O6).
start-agent:
    scripts/start-agent.sh
# Drive the agent yourself — one prompt = one turn (terminal 1).
talk-agent:
    scripts/talk-agent.sh
# Live agent status: which host holds the VM + turns served (terminal 2).
agent-status:
    scripts/agent-status.sh
# Auto-migrate at the next idle gap (waits out in-flight turns).
migrate-when-idle:
    scripts/migrate-when-idle.sh

# Live-migration demo (reads .env): a stateful in-RAM app survives an A->B move.
# prepare = fresh VM; long-process (terminal 1) = client load; status (terminal 2,
# live) = prints on change; migrate (terminal 2) = move the VM. See scripts/.
prepare:
    scripts/prepare.sh
long-process:
    scripts/long-process.sh
# Burst client: turns of load with random 3-10s idle gaps — migrate during a gap.
long-process-burst:
    scripts/long-process-burst.sh
migrate:
    scripts/migrate.sh
demo-status:
    scripts/status.sh

# Prometheus + Grafana stack (Grafana at http://localhost:3000). Edit
# deploy/prometheus/targets.json (gitignored) to point at your hostd daemons.
observe:
    cp -n deploy/prometheus/targets.json.example deploy/prometheus/targets.json 2>/dev/null || true
    cp -n deploy/prometheus/node-targets.json.example deploy/prometheus/node-targets.json 2>/dev/null || true
    docker compose -f deploy/docker-compose.yml up

# Tear the observability stack down.
observe-down:
    docker compose -f deploy/docker-compose.yml down
