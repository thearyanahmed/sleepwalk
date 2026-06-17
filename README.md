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

## Benchmark (cross-node, compatibility-class-matched, 20 runs)

A 256 MB guest migrated A→B between two **CPU-homogeneous** droplets (matching
TSC, same [compatibility class](docs/adr/0004-cpu-tsc-compatibility-classes.md)),
20 consecutive runs, **20/20 succeeded**. Source cost = pause → snapshot →
transfer-complete.

| metric (source cost) | min | max | mean | median |
|----------------------|----:|----:|-----:|-------:|
| snapshot (ms)        | 306 |  443 |  372 |    372 |
| transfer 256 MB (ms) | 963 | 1130 | 1011 |    969 |
| total (ms)           |1272 | 1513 | 1383 |   1349 |

Conditions: two DigitalOcean droplets, **2 vCPU** each, Firecracker v1.16.0,
kernel 6.8, 256 MB guest, snapshot on disk-backed `/tmp`, streamed over the
**public internet**, 20 runs. Source-side only (excludes target restore/resume).

**End-to-end continuity & user-perceived downtime (the headline).** A stateful
in-RAM HTTP app (the [`ramstate`](examples/ramstate) example — a counter living
only in process memory) was hit in a tight loop from a laptop, through a host port
(DNAT), *while* the VM migrated A→B, and the client-visible gap (last good
response before → first good response after) was measured across repeated runs.
**Every run that completed kept the same process `boot_id` and a monotonic
counter — zero state loss.**

| user-perceived downtime (continuity-preserved runs) | value |
|-----------------------------------------------------|------:|
| min                                                 | 0.17 s |
| median                                              | 2.40 s |
| mean                                                | 2.23 s |
| max                                                 | 7.93 s |

The gap is **bimodal**: **~0.17 s** when the VXLAN forwarding table is already
warm, **~3.8 s** on a cold FDB/ARP relearn (one 7.9 s outlier). The *freeze*
itself — snapshot (~435 ms) + 256 MB transfer (~989 ms) ≈ **1.4 s** — is only
part of it; the rest is **overlay relearn**, which a GARP-on-resume would both
shrink and stabilise. The freeze scales with **guest RAM, not app size** (the
whole 256 MB ships regardless of the ~1 KB the counter uses); diff snapshots,
smaller RAM, and a private link are the other levers (see Limitations).

*Methodology note: of 20 runs, 14 produced a clean continuity measurement; 6 hit
test-harness artifacts (intermittent ssh stalls in the driver + a duplicate-IP
collision from leftover VM state during a stall) — the migrations' snapshot +
transfer still succeeded each time, only the client's post-move reachability was
affected. A production driver (GARP + a clean per-run target) removes that noise.*

## Live coding-agent migration (interactive, observed)

The continuity benchmarks above move a synthetic in-RAM counter. This moves a
**workload that does externally-visible work**: a coding agent
([aider](https://aider.chat/), driven against a hosted model endpoint) running
inside the microVM, editing a source tree turn-by-turn while a human drives it
over HTTP — and the VM is migrated to the other host *mid-session, between turns*.

Setup: the agent rootfs runs an HTTP server where one `POST /ask` = one agent
turn (the model call + the file edits it makes). The model API key is handed to
the guest at boot over the Secrets vsock message — never in the image, argv, or a
committed file. A host port is DNAT'd to the guest so you drive it from a laptop.
The mechanics: `./scripts/start-agent.sh` boots it on A; `./scripts/talk-agent.sh`
sends a turn; `./scripts/agent-status.sh` shows which host holds the VM + the turn
count; `./scripts/migrate-when-idle.sh` waits for an idle gap and moves it.

**Idle-gap detection with no protocol change.** The agent's HTTP server is
single-threaded, so while it runs a turn it cannot answer a probe. The migrator
does a fast `GET /`: an answer means idle → migrate; a timeout means mid-turn →
wait. So a move is only attempted between turns, and a receiver is never orphaned
by a stood-down send.

Observed on a single interactive move (1024 MB guest, two DigitalOcean droplets,
public internet):

| source cost (one observed move) | value |
|---------------------------------|------:|
| snapshot                        | 4.42 s |
| transfer (1 GiB)                | 4.23 s |
| freeze ≈ snapshot + transfer    | ≈ 8.6 s |

The in-process turn counter and the agent's working tree carried across the move;
the agent answered its next turn — including a fresh model call — on the new host,
with no session reset. A migration fired *mid-turn* stands down (the gate holds)
and leaves the VM running on the source. The freeze scales with **guest RAM** (the
whole gig ships), not with what the agent is doing — the same diff-snapshot /
post-copy levers in [Limitations](#limitations) apply, and matter more here
because the guest is larger.

This is a single observed run, not a 20-cycle benchmark — it demonstrates
continuity of a stateful, side-effecting workload across a live host move, not a
steady-state freeze distribution.

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

## What we achieved

- A **running Firecracker microVM relocated across two physical hosts** —
  snapshot → network transfer → userfaultfd lazy restore — gated on *verified*
  quiescence. No Firecracker fork, no kernel patches, Apache-2.0.
- **Zero state loss across the move.** An in-RAM workload's state (a counter +
  heap) survives, the *same* process resumes on the target, and a live client
  connection follows the VM to the new host (the overlay keeps its IP).
- **Quiescence is verified, not assumed** (app + infra + storage layers), and a
  normative **race rule** guarantees an in-flight turn is never cut by a migration.
- **CPU/TSC compatibility classes** as a first-class placement constraint — the
  rebalancer refuses a cross-class move *up front* instead of failing at restore.
- A **live fleet view** (Prometheus + Grafana): servers by IP, machine
  resources, VM placement (host/IP flips on migration), and request rate holding
  through the freeze.
- **20/20** cross-node migrations; **~1.4 s mean source cost** for a 256 MB guest
  over the public internet.
- A **live coding agent** (aider against a hosted model) **migrated host-to-host
  mid-session, between turns** — its turn state and working tree rode the
  snapshot and it answered the next turn, with a fresh model call, on the new host.

## Pros & cons

**Pros**
- No Firecracker fork, no kernel patches, permissive licence (Apache-2.0).
- Externalized-state workloads (agents, jobs) relocate with **zero data loss**.
- Restore-side freeze is independent of total guest RAM — UFFD faults in only the
  pages the guest touches.
- Quiescence-gated: no in-flight work is ever interrupted.
- Compatibility-class-aware placement: migrations that would fail are never tried.

**Cons / trade-offs**
- **Not true live migration** — there is a real pause (the snapshot is
  stop-the-world), ~1.4 s source cost for a 256 MB guest over public internet.
  But the pause lands **only in the idle gaps between turns** (quiescence-gated),
  so it never interrupts a running turn. The design target is "**freeze fits the
  gap**," not "sub-100 ms freeze" — a turn racing the freeze sees a bounded
  start-delay, never a cut. Driving the freeze lower (diff snapshots, tmpfs,
  post-copy) widens the gaps it fits in; it isn't a correctness requirement.
- The pause + transfer scale with **guest RAM, not workload size**: a tiny app in
  a 256 MB VM still ships 256 MB. Diff snapshots are the fix.
- Snapshots are **CPU-compatibility-bound** (same vendor/model, TSC within ~250
  ppm); a heterogeneous cloud fleet fragments into classes.
- No live TCP migration; relocation only at quiescence.
- Pre-1.0; Linux/KVM + x86_64 only (see Limitations).

## Where sleepwalk sits vs. live migration

sleepwalk does **snapshot/restore at quiescence**, not true live migration. For
context (all sourced from upstream docs/repos):

- **Upstream Firecracker has no live migration** — only paused snapshot/restore;
  the microVM must be stopped to snapshot.
- **Loophole Labs Drafter/Silo** *did* do live migration (hybrid pre/post-copy)
  on Firecracker — but it required a **Firecracker fork + kernel (PVM) patches**,
  was **AGPL-3.0**, and is now **archived** (2025). Self-reported downtime
  <100 ms same-datacentre, ~500 ms intercontinental.
- **Cloud Hypervisor** (a sibling Rust VMM) has mature upstream **pre-copy** live
  migration via dirty-page tracking — but it isn't Firecracker.
- **Post-copy via userfaultfd is achievable on *stock*, Apache-2.0 Firecracker.**
  Firecracker hands the UFFD page-fault handler a raw file descriptor + the guest
  memory layout, then steps out of the way — so the handler is free to source
  pages **over the network** from the source host on demand. That turns
  sleepwalk's "restore from a local file" into "resume immediately, fault each
  page from the source as the guest touches it" — dropping the freeze toward the
  CPU/device-state switchover cost, independent of RAM size. **This is the natural
  next step and needs no fork.** True **pre-copy** (streaming dirty pages from a
  *running* VM) is **not** reachable without forking — the public API can't read
  dirty pages from a running microVM (that's the line Drafter crossed).
- Trade-off of post-copy: once the target faults pages in, no single host holds a
  complete image, so a mid-fault network partition loses the VM — the source must
  keep its snapshot until cutover for a safe fallback.

This is the gap sleepwalk occupies: the **permissive, no-fork, quiescence-gated**
point on the spectrum, with a clear post-copy path to lower downtime.

## Terminology

- **VM** — virtual machine. **microVM** — a minimal, fast-booting VM (Firecracker's unit).
- **VMM** — virtual machine monitor (the user-space hypervisor process; Firecracker is one).
- **KVM** — Kernel-based Virtual Machine, the Linux hardware-virtualization layer Firecracker runs on.
- **Firecracker / FC** — AWS's minimal VMM for microVMs.
- **snapshot / restore** — serialize a *paused* VM's memory + device state to files, recreate it later.
- **UFFD (userfaultfd)** — a Linux syscall that delivers page-fault events to a user-space handler; used here to restore guest memory **lazily** (pages load on first touch).
- **lazy restore** — resume the VM immediately and fault memory pages in on demand, instead of loading all RAM up front.
- **quiescence** — a verified idle state (no in-flight work) where it is safe to snapshot. **drain** — gate new work and wait for in-flight work to finish, reaching quiescence.
- **TSC** — Time Stamp Counter, a per-CPU cycle counter the guest uses as its clock; its frequency must match (or be hardware-scalable) across hosts for a snapshot to restore.
- **TSC scaling** — a CPU feature that presents a different TSC rate to the guest; needed to restore across mismatched TSC frequencies.
- **compatibility class** — the set of hosts a snapshot can move between (same CPU vendor/model, TSC within ~250 ppm, same kernel tier).
- **ppm** — parts per million (the TSC-match tolerance is 250 ppm).
- **vsock (AF_VSOCK)** — a socket family for host↔guest communication; the guest protocol runs over it. **CID** — Context ID, a vsock address identifying a VM.
- **tap** — a virtual L2 network interface on the host backing the guest's NIC. **bridge** — a virtual L2 switch (`br-sw`) the taps attach to.
- **VXLAN** — an L2-over-UDP overlay that spans the bridge across hosts, so a VM keeps its MAC/IP after moving.
- **FDB** — forwarding database (a bridge/VXLAN MAC→port table); it must relearn the VM's location after a move. **GARP** — gratuitous ARP, an unsolicited announcement that speeds that relearn.
- **NAT** — network address translation. **MASQUERADE** — the iptables NAT form that rewrites source addresses (egress / hairpin). **DNAT** — destination NAT, used to forward a host port to the guest so a client can reach it.
- **RPS** — requests per second. **p50 / p99** — median / 99th-percentile latency.
- **pre-copy / post-copy** — live-migration strategies: copy memory *before* vs. *after* switching the guest to the target host.
- **ADR** — Architecture Decision Record (see [`docs/adr/`](docs/adr/)).
- **rebalancer** — the control plane that picks and drives migrations. **hostd** — the per-host daemon. **guestd** — the in-VM supervisor (PID 1).

## License

Apache-2.0.
