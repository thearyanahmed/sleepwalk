# Overview & the state machine

A migration has two halves: a **source** function that drains, snapshots, and streams
a VM, and a **target** function that receives, restores, and resumes it. Between them
the control plane walks a **finite state machine** (FSM) whose legal transitions are
encoded in the type system.

## The state machine

```
Stable ─▶ Intent ─▶ Draining ─▶ Quiescent ─▶ Snapshotting ─▶ Transferring
  ▲          │          │                                          │
  │          └─ abort ──┴── (timeout / turn-in-flight too long)    │
  │                                                                ▼
Cleanup ◀── CutOver ◀── Restoring ◀────────────────────────────────┘
```

| State | Meaning |
|-------|---------|
| `Stable` | No migration in progress (the resting state). |
| `Intent` | The rebalancer decided to move this VM but has done nothing yet. |
| `Draining` | Drain requested; new turns are gated, waiting for quiescence. |
| `Quiescent` | All three quiescence layers satisfied; safe to snapshot. |
| `Snapshotting` | Pausing the VM and writing the snapshot. **Past the point of no abort.** |
| `Transferring` | Streaming the snapshot to the target host. |
| `Restoring` | Target restoring via UFFD lazy restore. |
| `CutOver` | Switching authority to the target: re-plumb the tap, release queued turns. |
| `Cleanup` | Tearing down source-side state (snapshot dir, FC process). |
| `Aborted` | Migration aborted before snapshotting; the VM stays put on the source. |

## Two representations, on purpose

The `proto::fsm` module models the FSM **twice**, deliberately:

- **`Migration<S>` — a typestate.** The phase is a *type parameter*. The only methods
  that exist on a value are the transitions legal from its phase. `snapshot()` exists
  on `Migration<Quiescent>` and nowhere else, so *snapshotting before quiescence is a
  compile error, not a runtime check*. Each transition **consumes** `self`, so a stale
  handle to a past phase cannot be reused by mistake.
- **`MigrationState` — a plain enum.** The same states as runtime values, for the
  things that need to store or serialize a state: structured-log transcripts, the
  Grafana FSM gauge, the `/metrics` endpoint.

```rust
// Legal — the types line up:
let m = Migration::intent(vm, src, dst)   // Migration<Intent>
    .drain()                              // Migration<Draining>
    .quiesce()                            // Migration<Quiescent>
    .snapshot();                          // Migration<Snapshotting>

// Illegal — does not compile, because `snapshot()` does not exist on Intent:
let m = Migration::intent(vm, src, dst).snapshot();   // ❌ compile error
```

## The point of no abort

`abort()` is implemented only for the `Abortable` phases — `Intent`, `Draining`, and
`Quiescent` — and returns the VM to `Stable` on the source host. **Once `Snapshotting`
begins, no `abort` method exists**: the migration runs to completion or fails over to
resume-on-source (the snapshot still exists on A). The type system enforces this; you
cannot even write the illegal call.

Every transition is emitted as a structured JSON event into the run transcript
(`results/`) and surfaced on the rebalancer's `/metrics` as an FSM gauge.

## The roles in code

| Function (`hostd::migrate`) | Role |
|-----------------------------|------|
| `migrate_running(vm, addr)` | Source side for a registered, running VM — the daemon path. |
| `migrate_source(art, addr)` | Source side that boots a fresh VM first — the benchmark path. |
| `restore_register(fc_bin, listener)` | Target side; returns the live VM for the daemon to register. |
| `restore_target(fc_bin, listener)` | Target side; one-shot for the benchmark CLI (restores then tears down). |

The next pages walk the [source side](source.md) and the
[target side & UFFD lazy restore](target-uffd.md) in detail.
