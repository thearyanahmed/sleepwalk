# The drain protocol & race rule

The race rule is the safety claim at the heart of sleepwalk: a migration may **never**
drop a turn or interrupt one in flight. It is normative — stated in the
[protocol](../protocol.md), enforced in code, and validated by a chaos test.

## The precedence

> **in-flight turn > migration > queued turn**

1. The rebalancer sends `DrainRequest`. From that instant `guestd` **gates** new turn
   starts.
2. If a turn was already in flight when the drain arrived, **it wins**: the migration
   waits up to `deadline_ms`. On timeout it `DrainCancel`s back to `Stable`. The turn
   is never sacrificed.
3. New turns that arrive *after* the drain are **queued in-guest**, not dropped — held
   as backlog and **replayed** once the gate reopens (after `Resumed` on the target, or
   after a `DrainCancel` on abort).
4. A turn-start that races the drain in the same instant resolves by the guest's local
   processing order: if `TurnStarted` was emitted before `DrainRequest` was handled, it
   counts as in-flight and wins.

"Zero dropped turns" is exactly this: a gated turn is **deferred, never lost**.

## The drain handshake

```
rebalancer/hostd                              guestd
      │                                          │
      │ DrainRequest { deadline_ms } ───────────▶│  gate new turns
      │                                          │  check in-flight
      │ DrainAck { in_flight: null } ◀───────────│  null ⇒ gated & idle
      │   └─ proceed to snapshot                 │
      │                                          │
      │   ── or ──                               │
      │ DrainAck { in_flight: 7 } ◀──────────────│  turn 7 still running
      │   └─ wait up to deadline_ms              │
      │       then DrainCancel ─────────────────▶│  un-gate, release queued turns
```

- `DrainAck { in_flight: null }` ⇒ new turns gated **and** none running ⇒ app-layer
  quiescent ⇒ snapshot may proceed.
- `DrainAck { in_flight: <turn_id> }` ⇒ wait for that turn (or time out).
- `DrainCancel` ⇒ migration aborted; un-gate and replay any queued turns.

The `null` is always present, never omitted; absence of an in-flight turn is `null`,
never turn `0`.

## How it shows up on the source side

On the [source side](../migration/source.md), a `Busy` drain result is not an error —
it returns `MigrateOutcome::StoodDown(vm)`, handing the VM back intact for the caller
to re-register and retry at the next idle gap. The in-flight turn runs on, undisturbed.

## Wrap mode is passive

In [wrap mode](../protocol.md) `guestd` only *observes* turn boundaries (from the
child's stdout); it cannot defer a turn the child has already begun. So drain is
**passive**: the host simply waits until the child is between turns before
snapshotting. New turns are not gated or queued — that active guarantee is native-mode
only.

## Validation: the chaos test

The race rule is falsified, not just asserted, by `harness`'s chaos test
(`just chaos`): a drain is dropped at random offsets across a stream of turns, over
many **seeded** interleavings (deterministic — a failure prints the reproducing seed,
no VM required). It asserts:

1. **Every attempted turn eventually runs** — zero dropped.
2. **The `DrainAck`'s `in_flight` matches the turn actually running** at the drain
   instant.
3. **No turn starts while the gate is closed.**

The KVM wall-clock counterpart (`just chaos-vm`, 100 runs on `/dev/kvm`) is the
integration-tier version of the same property.
