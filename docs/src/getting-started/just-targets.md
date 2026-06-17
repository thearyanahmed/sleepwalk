# The `just` target map

`just` is the single entry point for every action; there are no undocumented manual
steps. Targets are grouped by the **dev tier** they require (see
[Development environment](environment.md)):

- **tier 1 · host-agnostic** — runs anywhere, including macOS native (no VM).
- **tier 2 · functional KVM** — needs `/dev/kvm` (path A, A′, B, or C).
- **tier 3 · real KVM only** — benchmark-valid hosts (path A, B, C — never A′/TCG).

Many tier-2/3 targets are stubs that fail loud until their capability lands.

## tier 1 — host-agnostic

| Target | What it does |
|--------|--------------|
| `just test` | Unit + mock tests across the workspace. |
| `just lint` | `cargo fmt --check` + `clippy --all-targets -D warnings`. |
| `just fmt` | Apply `rustfmt`. |
| `just chaos` | Turn-vs-drain race-rule chaos over seeded interleavings (deterministic, no VM). A failure prints the reproducing seed. |
| `just fetch` | Download + checksum the pinned Firecracker binary and kernel. |
| `just dev-vm` / `dev-vm-ssh` / `dev-vm-setup` | Boot / shell / provision the M1/M2 TCG dev VM (path A′). |
| `just remote-sync` / `remote-ssh` / `remote-setup` / `remote-run` | Drive a remote host over SSH (config in gitignored `.env`). |
| `just hostd-daemon [ADDR]` | Run the per-host daemon (serves `/healthz` + `/metrics`). |
| `just guest-rootfs` | Build the synthetic guest rootfs (static guestd as init). |
| `just agent-rootfs` | Build the agent guest rootfs (Ubuntu + aider, guestd as init; needs root). |
| `just uffd-test` | UFFD lazy-restore page server (Linux `userfaultfd`, no KVM). |
| `just vsock-test` | `AF_VSOCK` transport round-trip over loopback (Linux, no KVM). |

## tier 2 — functional KVM

| Target | What it does |
|--------|--------------|
| `just lifecycle-test` | Single-host Firecracker boot lifecycle. |
| `just restore-test` | Single-host snapshot → UFFD lazy restore of a live VM. |
| `just migrate-bench` | Freeze-window benchmark: boot once, ping-pong N migrations, record JSON. Tunable via `SLEEPWALK_BENCH_CYCLES` / `SLEEPWALK_BENCH_SETTLE_MS`. |
| `just migrate-recv ADDR [N]` / `migrate-send ADDR [N]` | Two-process A→B migration (start the receiver first). |
| `just migrate-test` | A→B migration with memory streamed over TCP (loopback; point the sender at another host's IP for cross-host). |
| `just chaos-vm` | Turn-vs-drain chaos against live VMs (stub). |

## tier 3 — real KVM only (refuses TCG)

| Target | What it does |
|--------|--------------|
| `just bench-restore` | O2 freeze-window vs RAM table (stub). |
| `just e2e` | Full fleet scenario, O5 (stub). |

## Demos

| Target | What it does |
|--------|--------------|
| `just start-agent` / `talk-agent` / `agent-status` / `migrate-when-idle` | The agent demo (O6): a coding agent survives a mid-session migration. |
| `just prepare` / `long-process` / `migrate` / `demo-status` | The synthetic demo: a stateful in-RAM app survives an A→B move. |
| `just observe` / `observe-down` | Prometheus + Grafana stack (Grafana at `http://localhost:3000`). |

See **[Demos](../operations/demos.md)** for the step-by-step walkthroughs.
