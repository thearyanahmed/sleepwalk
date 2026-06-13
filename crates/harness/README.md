# `harness`

Load generator, latency recorder, and chaos harness. Internal crate. This first
slice is the **measurement math** — the part that must be right for any published
number to be trustworthy.

## What's here

| Module     | Contents |
|------------|----------|
| `schedule` | `Schedule` / `Arrivals` — the open-loop arrival schedule (fixed-rate or Poisson), computed up front. Poisson is seeded, so runs reproduce exactly. |
| `recorder` | `LatencyRecorder` / `LatencyStats` — latency measured from the *intended* send time, aggregated into an HdrHistogram for accurate tail percentiles. |

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
