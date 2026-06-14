# `rebalancer`

The control plane: decides which VM moves where, and drives each migration to
completion. Internal crate. This slice covers the host-agnostic **decision**
(pick-victim heuristic) and the **migration driver**.

## What's here

| Module            | Contents |
|-------------------|----------|
| `placement`       | `Placement` / `Pressure` / `pick_victim` — the pressure-relief heuristic. Picks the most-idle VM on the hottest host and moves it to the coolest host that can take it; returns `None` (rather than guessing) when no move helps. One move at a time. |
| `executor`        | `MigrationExecutor` — the port for migration effects (`request_drain`, `snapshot`, `transfer`, `restore`, `cutover`, `cleanup`), plus `DrainOutcome` / `ExecError`. |
| `driver`          | `drive` — walks proto's migration FSM typestate through the executor, returning `MigrationOutcome`. |
| `pseudo_executor` | `PseudoExecutor` — a recording, fault-injecting stand-in implementing the same trait, for tests. |

## Design

- **The driver owns the order, the executor does the work.** `drive` decides
  *what* happens next (the legal FSM transition); the executor performs the
  effect against hostd. The real executor drives the control plane; tests use
  `PseudoExecutor`.
- **The race rule is enforced at the gate.** If `request_drain` returns
  `DrainOutcome::Busy`, the migration aborts to `Stable` and the in-flight turn
  is never cut — the turn always wins.
- **The point of no return is a type guarantee.** Once `snapshot` has run, proto's
  typestate offers no `abort`, so a later executor failure propagates as a
  `MigrationError` (fail-over to resume-on-source is a later slice) — it cannot
  silently roll back.

## Testing it in isolation

```
cargo test -p rebalancer   # the driver against PseudoExecutor — full path, abort, failures
```
