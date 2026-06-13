# `guestd`

The in-VM supervisor — hostd's representative inside each microVM. Runs as the
guest half of the vsock protocol: announces the VM at boot, takes secrets over
the wire (never the rootfs or kernel cmdline), reports turn boundaries so hostd
can verify quiescence, and holds the drain gate that makes "migrate only at a
safe point" actually safe. Pairs with [`hostd`](../hostd). Internal crate.

## What's here (first slice)

| Module    | Contents |
|-----------|----------|
| `channel` | `GuestChannel` — the vsock seam (`send`/`recv`) — plus `FakeChannel`, a scripted, recording fake for tests. |
| `guest`   | `Guest` — the supervisor state machine: boot handshake, turn signals (`TurnStarted`/`TurnEnded`), and the drain gate (`DrainRequest` → `DrainAck`). |

## Design

- **Traits as ports.** All vsock I/O is behind `GuestChannel`, so the supervisor
  logic tests in milliseconds with no real vsock. The `AF_VSOCK` implementation
  slots in behind the same trait.
- **The drain gate is the race rule, guest side.** A turn already in flight when
  `DrainRequest` arrives is reported in the `DrainAck` (`in_flight: Some`) and is
  never cut short; turns that arrive after the gate closes are refused
  (`StartOutcome::Gated`) to be replayed after resume. A drain found idle acks
  `None` — the app layer is quiescent.
- **Secrets stay in memory.** Received via `Secrets` at boot and kept in the
  process env only — never persisted.

## Not here yet (needs a running guest)

The real `AF_VSOCK` transport and wrapping an actual workload process require a
booted microVM; they land in a later slice, exercised by `just lifecycle-test`.

## Testing it in isolation

```
cargo test -p guestd       # handshake, turn signals, drain gate — all via FakeChannel
```
