# `proto`

The public contract of sleepwalk: the wire types the guest and host exchange,
and the migration state machine the rebalancer drives. Pure types — no I/O, no
async — so it builds and tests anywhere (`just test`, tier 1).

Internal workspace crate; **not** published. The versioned _document_
([`docs/protocol.md`](../../docs/protocol.md)) is the integration surface (O8),
not a Rust dependency — a non-Rust guest reads the doc, not this crate.

## What's here

| Module  | Contents |
|---------|----------|
| `ids`   | Identifier newtypes — `VmId`, `HostId`, `TurnId`, `GuestdVersion`, `Timestamp`. No stringly-typed domain values, no sentinels (absence is `Option`). |
| `vsock` | The guestd ⇄ hostd protocol: `GuestToHost` / `HostToGuest`, split by direction so an illegal message is unrepresentable. Newline-delimited JSON. |
| `fsm`   | The migration state machine as a **typestate** (`Migration<S>`) plus a runtime `MigrationState` enum for logs/metrics. |

`PROTOCOL_VERSION` (`v1-draft`) pins the wire shape until the v0.1.0 API freeze.

## Key design choices

- **Direction-typed messages.** A guest can't construct `Secrets`; hostd can't
  forge `TurnStarted`. The split enums make wrong-direction sends a type error.
- **Typestate FSM.** `snapshot()` exists only on `Migration<Quiescent>`, so
  snapshotting before quiescence won't compile. `abort()` exists only on
  pre-snapshot phases — a `compile_fail` doctest pins that it's unavailable once
  memory has been dumped.
- **Time at the boundary.** Rust holds `Duration`; the wire carries integer
  milliseconds (`deadline_ms`). `Timestamp` is integer nanoseconds since the
  epoch and is guest-sourced — _not_ comparable across a migration until the
  guest's clock fix-up on `Resumed`.

## Testing it in isolation

```
cargo test -p proto            # unit + doctests (round-trips, FSM walk, compile_fail)
cargo test -p proto --doc      # just the doctests, incl. the typestate negative
```
