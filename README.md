# sleepwalk

Zero-perceived-downtime rebalancing for [Firecracker](https://firecracker-microvm.github.io/) microVMs: relocate a running VM between hosts by snapshotting it, transferring the memory, and lazily restoring it on the target via [userfaultfd](https://man7.org/linux/man-pages/man2/userfaultfd.2.html) — gated on *verified* workload quiescence, so the VM is paused during a real idle gap, moved, and wakes on another host none the wiser. Built for agent-sandbox and job-shaped workloads whose state is externalized and whose turns have natural pauses; no Firecracker fork, no kernel patches, Apache-2.0.

**Status:** pre-alpha, pre-`v0.1.0` — under active construction, nothing here is stable yet. Run `just --list` for the current entry points.

## Architecture

A `rebalancer` control plane drives a `hostd` daemon on each host. Each `hostd`
runs Firecracker microVMs; inside each VM, `guestd` (PID 1) speaks the guest
protocol over vsock — reporting turn boundaries, holding the drain gate, taking
secrets at boot. When a host gets hot, the rebalancer picks an idle VM and moves
it to a cooler host: snapshot on the source, stream memory + vmstate over the
network, lazily restore on the target via userfaultfd. The move happens only at
*verified quiescence*, so no live turn is ever interrupted.

```
                         ┌──────────────────────────────────────────┐
                         │                rebalancer                  │
                         │  placement · pressure · migration FSM      │
                         └───────┬───────────────────────────┬───────┘
                            HTTP │                            │ HTTP
                  ┌──────────────▼─────────────┐  ┌───────────▼────────────────┐
                  │          hostd (A)          │  │          hostd (B)          │
                  │  Firecracker API · UFFD     │  │  Firecracker API · UFFD     │
                  │  snapshot/transfer · drain  │  │  snapshot/transfer · drain  │
                  └──────────────┬──────────────┘  └─────────────┬───────────────┘
                           vsock │                                │ vsock
                  ┌──────────────▼──────────────┐  ┌─────────────▼───────────────┐
                  │          microVM            │  │          microVM            │
                  │  ┌────────────────────────┐ │  │  ┌────────────────────────┐ │
                  │  │ guestd (PID 1)         │ │  │  │ guestd (PID 1)         │ │
                  │  │  └─ workload / turns   │ │  │  │  └─ workload / turns   │ │
                  │  └────────────────────────┘ │  │  └────────────────────────┘ │
                  └─────────────────────────────┘  └─────────────────────────────┘
                                 │                                ▲
                                 │   snapshot stream (mem + vmstate, TCP)
                                 └────────────────────────────────┘
                                       relocate at quiescence
```

### Workspace crates

| crate | role |
|-------|------|
| `proto` | the public contract: vsock messages, hostd API, the migration state machine as a typestate (illegal transitions don't compile) |
| `guestd` | in-VM supervisor (PID 1): vsock handshake, turn signals, drain gate, boot secrets, post-restore clock fix-up. Native mode (workload speaks the protocol) or wrap mode (turn boundaries inferred from a wrapped process's stdout) |
| `hostd` | per-host daemon: Firecracker lifecycle, the userfaultfd page server, snapshot transfer, the layered quiescence detector, `/metrics` |
| `rebalancer` | control plane: host pressure, victim selection, and the migration FSM driver |
| `harness` | open-loop load generator + latency recorder; also drives the turn-vs-drain chaos test |
| `cli` | the `sleepwalk` binary — the published front door wrapping the daemons |

### How a migration works

The migration state machine is owned by the rebalancer; the typestate makes
quiescence a precondition of snapshotting at compile time.

```
Stable ─▶ Intent ─▶ Draining ─▶ Quiescent ─▶ Snapshotting ─▶ Transferring
  ▲          │          │                                          │
  │          └─ abort ──┴── (timeout / turn-in-flight too long)    │
  │                                                                ▼
Cleanup ◀── CutOver ◀── Restoring ◀────────────────────────────────┘
```

Abort is legal anywhere **before** `Snapshotting` and returns the VM to `Stable`
on the source; once snapshotting starts, the migration runs to completion or
resumes on the source. **Quiescence is verified, not assumed** — a VM is movable
only when all three layers agree: the app layer (guestd has acked the drain with
no turn in flight), the infra layer (vCPU + virtio queues quiet), and the storage
layer (workspace sync caught up). The **race rule** is normative: an in-flight
turn beats a migration, which beats a queued turn — a turn is never sacrificed to
a move. Full message-level detail is in
[`docs/protocol.md`](docs/protocol.md).

## Local development

Firecracker needs KVM, so development happens inside a Linux VM with `/dev/kvm`. On
Apple Silicon without hardware nested virtualization (M1/M2), the local dev VM runs
under QEMU's software CPU emulator (TCG), which boots the full stack correctly but
~10–30× slower — fine for development and correctness, **never valid for benchmarks**.
See [`docs/environment.md`](docs/environment.md) for the supported dev paths (native
KVM on M3+/x86/remote, TCG on M1/M2) and the rationale.

## Benchmark (preliminary, single-host)

Freeze window from `just migrate-bench`: boot one microVM, migrate it 20 times
(snapshot → userfaultfd lazy restore → resume), timing the paused window.

| metric | value |
|--------|------:|
| migrations | 20 |
| min freeze | 357 ms |
| max freeze | 1458 ms |
| mean freeze | 1183 ms |
| guest RAM | 256 MB |

Conditions: Firecracker v1.16.0, guest kernel 6.1.155, 1 vCPU / 256 MB guest,
single host (snapshot files on local disk, no network transfer), 20 cycles, 1 s
settle between, snapshot file on disk-backed `/tmp`.

Caveats: single-host, so no memory crosses a network. The freeze is dominated by
writing the full 256 MB RAM dump during the pause — userfaultfd accelerates
restore, not snapshot; diff snapshots and a tmpfs mem file are the levers to cut
it. On a 1-vCPU box with disk-backed `/tmp` these numbers are not representative
([environment matrix](docs/environment.md)).

## Benchmark (preliminary, two droplets)

A running microVM migrated between two separate droplets over the public network
— snapshot on A, memory + vmstate streamed over TCP to B, userfaultfd lazy
restore on B, guest resumes. 20 migrations, all succeeded; B restored 20/20.

| metric (source cost = snapshot + transfer) | min | max | mean |
|--------------------------------------------|----:|----:|-----:|
| snapshot (ms) | 234 | 882 | 284 |
| transfer 256 MB (ms) | 1269 | 2649 | 1467 |
| total (ms) | 1511 | 3531 | 1751 |

Conditions: two DigitalOcean droplets, Firecracker v1.16.0, kernel 6.1.155, 1
vCPU / 256 MB guest each, snapshot files on disk-backed `/tmp`, 20 runs (run 1 is
a cold-cache outlier; steady state ≈ 1.5–1.7 s). Generated by `migrate send …
20` against `migrate recv … 20`.

Caveats: this is the **source side only** (pause → snapshot → transfer-complete),
measured on one clock; the target's lazy restore/resume is not included, so it is
a lower bound on total perceived downtime. Transfer dominates and scales with
guest RAM — diff snapshots and a faster path are the levers. 1-vCPU droplets, so
not representative numbers.

## Limitations

- **Linux + KVM, x86_64** for the VM-facing paths. The host-agnostic logic builds
  and tests anywhere; booting/migrating a microVM needs `/dev/kvm`.
- **Snapshots are CPU-compatibility-bound.** A snapshot can only be restored on a
  host whose CPU is compatible with the one that took it — same vendor, and a
  **TSC frequency within ~250 ppm** (or a host with hardware TSC scaling).
  Firecracker rescales the guest TSC on restore via `KVM_SET_TSC_KHZ`; if the
  target lacks scaling and the frequencies differ, the restore fails. sleepwalk
  models this as a **compatibility class** per host (`vendor / model / TSC /
  kernel`, see [ADR-004](docs/adr/0004-cpu-tsc-compatibility-classes.md)) and the
  rebalancer only moves a VM **within a class** — a cross-class move is refused
  up front instead of failing at load. Note that on shared clouds the *same
  instance size can land on different physical CPUs*, so a fleet is naturally
  multi-class; class-aware placement is required, not optional. Cross-vendor
  (Intel↔AMD) live restore is never possible.
- **No live TCP migration.** Relocation happens at verified quiescence, when no
  connections are in flight — by design.
- **Pre-1.0.** CLI, config, and the guest protocol may change with a CHANGELOG
  entry and a version bump.

## License

Apache-2.0.
