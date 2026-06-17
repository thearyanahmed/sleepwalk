# Layered quiescence

A VM is **quiescent** — safe to snapshot and move — only when *all three* layers are
simultaneously quiet. The whole point is that quiescence is **verified, not assumed**:
every layer defaults to **active** (not quiet) until it has positive evidence
otherwise, so a missing signal never reads as "safe to migrate." This is objective O3,
and it lives in `hostd::quiesce`.

```
            ┌─────────────────────────────────────────────┐
            │              QuiescenceDetector               │
            │  is_quiescent() = app ∧ infra ∧ storage       │
            └───────┬─────────────┬──────────────┬──────────┘
                    │             │              │
              ┌─────▼────┐  ┌─────▼─────┐  ┌─────▼──────┐
              │ AppLayer │  │ InfraLayer│  │StorageLayer│
              └──────────┘  └───────────┘  └────────────┘
              gated &&       window full,    sync caught
              nothing        all samples     up to backing
              in flight      < cpu_pct,      storage
                             queues quiet
```

## 1. App layer — the workload itself

Ground truth from `guestd`: new turns are gated **and** none is in flight.

```rust
pub fn is_quiet(&self) -> bool {
    self.gated && self.in_flight.is_none()
}
```

`hostd` updates it from the guest's vsock stream — `drain_acked(in_flight)` closes the
gate, `turn_started` / `turn_ended` track the in-flight turn, `drain_cancelled`
reopens the gate. While a turn runs, the layer is **active** and no migration can
proceed — this is the app-layer half of the [race rule](race-rule.md).

## 2. Infra layer — the machine

Catches background work the app never reported (a stray `npm install`): vCPU
utilization has stayed below a threshold for **N consecutive samples**, *and* the
virtio queues are quiet.

```rust
pub fn is_quiet(&self) -> bool {
    self.thresholds.samples > 0
        && self.recent.len() == self.thresholds.samples  // window full
        && self.queues_quiet                              // I/O quiet
        && self.recent.iter().all(|&c| c < self.thresholds.cpu_pct)
}
```

It holds a sliding window of the most recent samples. It is quiet only once the window
is **full** and *every* sample in it is below `cpu_pct` — so a single CPU spike resets
the evidence and must age fully out of the window (it takes `samples` fresh quiet
readings, not fewer) before the layer reads quiet again. `cpu_pct` and `samples` are
config keys, tuned during measurement.

## 3. Storage layer — durable state

The workspace sync has caught up to backing storage:

```rust
pub fn is_quiet(&self) -> bool {
    self.caught_up
}
```

In the PoC this is an rsync-to-shared-dir watermark; it stands in for a production
versioned-filesystem sync. If durable state has not caught up, the layer is active.

## The verdict

```rust
pub fn is_quiescent(&self) -> bool {
    self.report().is_quiescent()   // app && infra && storage
}
```

`QuiescenceReport { app, infra, storage }` is a per-layer snapshot for logs, the
`/metrics` gauge, and the `sleepwalk quiesce <vm>` inspection command — so an operator
can see *which* layer is holding a migration back.

## Why "default to active" matters

Each layer starts **not quiet**:

- `AppLayer::new()` — not gated, so not quiet.
- `InfraLayer::new(..)` — no samples yet, so not quiet.
- `StorageLayer::new()` — not caught up, so not quiet.

A detector with no data is never quiescent. A lost signal, a crashed sampler, a guest
that never acked — all read as "not safe to move," never as "safe." Safety is the
default; quiescence has to be *earned* with evidence. This is enforced by unit tests
(`detector_is_quiescent_only_when_all_three_agree`, `infra_layer_spike_resets_the_evidence`).
