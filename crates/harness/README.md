# `harness`

Load generator, latency recorder, and chaos harness. Internal crate. This first
slice is the **measurement math** — the part that must be right for any published
number to be trustworthy.

## What's here

| Module     | Contents |
|------------|----------|
| `schedule` | `Schedule` / `Arrivals` — the open-loop arrival schedule (fixed-rate or Poisson), computed up front. Poisson is seeded, so runs reproduce exactly. |
| `recorder` | `LatencyRecorder` / `LatencyStats` — latency measured from the *intended* send time, aggregated into an HdrHistogram for accurate tail percentiles. |
| `chaos`    | `simulate` / `RaceReport` — the turn-vs-drain chaos harness. Drops a drain at a random offset into a stream of turns, drives a real `guestd` supervisor through the interleaving, then resumes and replays the backlog, asserting the race rule. |
| `report`   | `RunReport` / `render_markdown` — per-migration measurement records as a JSON artifact in, the `results/report.md` markdown tables (freeze window, e2e, clean-vs-overlapping turn latency, idle-gap histogram) out. Every number is rendered beside its methodology. |

## The race-rule chaos harness

The race rule (`docs/protocol.md`) promises a migration never drops a turn or
cuts one short. `chaos::simulate(seed)` falsifies that cheaply: it builds a
deterministic event timeline on an integer fake clock — sequential turns plus a
single `DrainRequest` at a random offset — runs a real `Guest` through it, and
returns a `RaceReport` checking three invariants:

1. **Zero dropped turns** — every attempted turn eventually completes (the
   in-flight winner finishes; gated turns queue and replay after resume).
2. **Correct winner** — the `DrainAck`'s `in_flight` matches what was actually
   running at the drain instant, derived independently from the wire transcript.
3. **No start while gated** — no turn begins after the gate closes.

Each run is fully determined by its `seed`, so a failure reproduces from the seed
alone. The bundled test sweeps thousands of seeds and asserts the corpus
exercises *both* a busy drain and an idle-gap drain (so it can't pass vacuously).
Run it with `just chaos`. This is the fast mock layer; the wall-clock real-VM
counterpart (100 runs on `/dev/kvm`, `just chaos-vm`) is the integration tier.

## Why it's built this way: coordinated omission

A closed-loop generator (send, wait for the response, send the next) silently
stops sending while a request is stalled — so it never samples the latency of
the stall. That hides exactly the spike a migration could cause. Two choices here
avoid it:

1. **Open loop.** The schedule of intended send times is fixed in advance,
   independent of completions, so a stall does not pause sending.
2. **Intended-time latency.** A request's latency is `completed - intended`, not
   `completed - actually_sent`, so a late send shows up as latency instead of
   vanishing.

No external RNG dependency: Poisson inter-arrivals use a small seeded SplitMix64,
which keeps the schedule deterministic and the dependency tree minimal.

## Testing it in isolation

```
cargo test -p harness   # schedule spacing/determinism, percentiles, intended-time accounting
```
