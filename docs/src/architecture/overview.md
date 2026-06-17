# System overview

Three planes, talking over two channels.

```
        ┌──────────────────────────────────────────────────────┐
        │                     rebalancer                          │  control plane
        │   placement map · pressure signals · migration FSM      │
        └───────┬──────────────────────────────────────┬─────────┘
            HTTP│                                    HTTP│
        ┌───────▼────────┐      snapshot stream    ┌────▼───────────┐
        │   hostd (A)     │ ─────────────────────▶ │   hostd (B)     │  host plane
        │  FC API · UFFD  │      (TCP, checksummed) │  FC API · UFFD  │
        └───────┬─────────┘                         └────┬───────────┘
           vsock│ / guest-net TCP                   vsock│ / guest-net TCP
        ┌───────▼─────────┐        relocated        ┌────▼───────────┐
        │   microVM        │  ═══════════════════▶  │   microVM       │  guest plane
        │  guestd (PID 1)  │                         │  guestd (PID 1) │
        │   └─ workload    │                         │   └─ workload   │
        └──────────────────┘                         └─────────────────┘
```

## The three planes

**Control plane — `rebalancer`.** Watches host memory pressure, holds the placement
map (which VM is on which host), picks a victim VM when a host gets hot, and drives
the migration [state machine](../migration/overview.md) host-to-host. It talks to
each `hostd` over HTTP.

**Host plane — `hostd`, one per physical host.** Owns Firecracker: spawns and jails
the VM process, drives the Firecracker API socket (boot, pause, resume, snapshot,
load), runs the [UFFD page server](../migration/target-uffd.md) on the restore side,
and streams snapshots between hosts. It also runs the
[layered quiescence detector](../quiescence/layers.md) and exposes `/healthz` +
`/metrics`.

**Guest plane — `guestd`, PID 1 inside each microVM.** Speaks the
[guest protocol](../protocol.md): the boot handshake, turn boundaries
(`TurnStarted` / `TurnEnded`), the drain gate (`DrainRequest` / `DrainAck` /
`DrainCancel`), secret handoff at boot, and the post-restore `Resumed` signal. It
either *wraps* an arbitrary command (zero-code adoption) or lets a workload speak the
protocol *natively* for exact turn boundaries.

## The two channels

- **vsock** — the boot/turn path between `guestd` and `hostd`. Newline-delimited
  JSON, one object per line, internally tagged with a `type` field.
- **Guest-network TCP** — the *same* protocol on a TCP port inside the guest. It
  exists because **Firecracker's vsock device stops servicing connections after a
  snapshot restore**, while the guest network survives. So a *restored* VM is drained
  over TCP — which is what makes re-migrating (moving an already-migrated VM again)
  possible.

## Why this shape

- The control/host/guest split keeps policy (when and where to move) out of mechanism
  (how to move), so the rebalancer's placement heuristic is pluggable without touching
  the migration path.
- `hostd` is the only thing that talks to Firecracker, so the FC version dependency is
  contained in one crate.
- The guest protocol — not a Rust library — is the integration contract. A non-Rust
  workload can speak it directly (objective O8).
