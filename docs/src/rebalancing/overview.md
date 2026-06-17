# Fleet rebalancing

The rebalancer is the control plane's brain: it watches a fleet of hosts, decides
**which** VM to move **from where to where**, and drives the migration. It does not
implement the move — it calls the same `hostd` `migrate_recv` / `migrate_send`
endpoints the [migration pipeline](../migration/overview.md) exposes. One decision,
one move, then it looks again.

## What it balances: pressure, not VM count

The signal is **memory pressure** per host, not the number of VMs. A host running
forty small VMs can be cooler than one running thirty large ones. In the walkthrough
below we assume **equal-size VMs**, so pressure is proportional to count and "30 VMs"
reads as "30 units of pressure" — that keeps the arithmetic legible. Real fleets feed
the actual per-host pressure reported by `/status`.

## One step: `rebalance_once`

Each step is a closed loop over the live fleet:

```
poll every host  →  pick_victim  →  recv on target  →  send on source  →  repeat
   /status          (decision)       (hostd)            (hostd)
```

`pick_victim` makes the decision with four guards, in order:

1. **Hottest host over the high watermark?** If the busiest host is still under the
   watermark, nothing is overloaded — return `None`, the fleet is balanced.
2. **A compatible target exists?** The snapshot is taken on the source and can only be
   restored on a host of a matching CPU/TSC class. No compatible host ⇒ no safe move.
3. **Is that target actually cooler?** If the coolest compatible host is not cooler
   than the source, a move would not relieve pressure — return `None`.
4. **Which VM?** The **most-idle** VM on the hottest host becomes the victim, so the
   move is least likely to disturb live work.

A step moves **exactly one** VM. Convergence is iterative: re-poll, decide, move,
until guard 1 says everyone is under the watermark.

## Walkthrough: 30 / 30 / 40 → 33 / 33 / 34

Three hosts, 100 equal-size VMs, all the same CPU class (so every move is
compatible). High watermark = **34**. The rebalancer runs `rebalance_once` in a loop;
each step picks the hottest host (C), sends its most-idle VM to the coolest host, and
re-polls.

```
              host A        host B        host C ← hottest
 watermark=34  (cool)        (cool)        (over)
              ───────       ───────       ───────
 start          30            30            40

 step 1   C ──────────────────────────────▶ A      (C hottest, A coolest)
                31            30            39

 step 2   C ─────────────────▶ B                    (C still hottest, B coolest)
                31            31            38

 step 3   C ──────────────────────────────▶ A
                32            31            37

 step 4   C ─────────────────▶ B
                32            32            36

 step 5   C ──────────────────────────────▶ A
                33            32            35

 step 6   C ─────────────────▶ B
                33            33            34

 step 7   poll → hottest = C (34) ≤ watermark (34) → pick_victim returns None → STOP
```

Six moves, all off host C, alternating between the two cool hosts because the *coolest*
target is recomputed every step. The loop halts the instant the hottest host is no
longer over the watermark — it does **not** chase a perfectly even split. `33/33/34`
is where "nobody is over the line" happens to land; the rebalancer is satisfied by the
watermark, not by symmetry.

## One move, component by component

Take **step 1** above — victim VM `v` moves from **host C** (source) to **host A**
(target). The cast:

- **`rebalancer`** — one process, somewhere on the network. Speaks **HTTP only, to
  hostd's**. It never talks to a guestd.
- **`hostd@C`**, **`hostd@A`**, **`hostd@B`** — one daemon per physical server.
- **`guestd(v)`** — PID 1 inside the victim VM. Only ever talks to its **local**
  hostd: `hostd@C` before the move, `hostd@A` after.

```
rebalancer        hostd@C (src)     hostd@A (tgt)     hostd@B      guestd(v)
    │                  │                 │               │            │
    │ 1 GET /status    │                 │               │            │   ── poll the
    ├─────────────────▶│                 │               │            │      whole fleet
    │ GET /status ─────┼────────────────▶│               │            │
    │ GET /status ─────┼─────────────────┼──────────────▶│            │
    │◀─ pressure,vms,compat (×3) ────────────────────────┤            │
    │                  │                 │               │            │
    │ 2 pick_victim()  │                 │               │            │   ── decide LOCALLY:
    │   C hottest, A coolest, v = most-idle on C          │            │      no calls
    │                  │                 │               │            │
    │ 3 POST /migrate/recv?listen=A:DATA  │               │            │   ── target first
    ├─────────────────┼────────────────▶│               │            │
    │                  │       hostd@A binds receiver,    │            │
    │                  │       spawns bg restore, returns │            │
    │◀── "receiving" ──┼─────────────────┤               │            │
    │                  │                 │               │            │
    │ 4 POST /migrate/send?vm=v&to=A:DATA │               │            │   ── source drains+
    ├─────────────────▶│                 │               │            │      sends. blocks
    │                  │  DrainRequest(deadline_ms)       │            │      till done
    │                  ├─────────────────┼───────────────┼───────────▶│
    │                  │◀─ DrainAck(null | in_flight) ────┼────────────┤   ── in-flight turn
    │                  │  (if in_flight: wait ≤ deadline) │            │      wins; may wait
    │                  │                 │               │            │
    │                  │  snapshot v, stream RAM+state ──▶│            │   ── TCP on DATA_PORT,
    │                  │  ════════════════════════════▶  │            │      C ⟶ A direct
    │                  │                 │  UFFD restore, │            │
    │                  │                 │  re-plumb tap, │            │
    │                  │                 │  resume VM     │            │
    │                  │                 │◀─ Resumed ─────┼────────────┤   ── guestd(v) now on A;
    │                  │                 │  (clock fixup) │            │      clock fix-up
    │                  │                 │  gratuitous ARP→ fleet       │   ── clients relearn MAC
    │                  │  drop local copy │               │            │
    │◀── timing JSON ──┤                 │               │            │   ── send returns
    │                  │                 │               │            │
    │ 5 loop → GET /status again …        │               │            │   ── next step
```

### When the agent is mid-turn (the wait)

Step 4's `DrainAck` has two outcomes. If `guestd(v)` answers `null`, it was already
app-quiescent and the snapshot starts immediately. If it answers a **turn id**, a turn
is in flight — the [race rule](../quiescence/race-rule.md) says that turn wins, and
`hostd@C` waits for it. The rebalancer is still blocked on the `send` POST throughout;
it has no idea the move is waiting on an agent.

```
rebalancer        hostd@C (src)                         guestd(v) — running turn 42
    │                  │                                     │
    │ (blocked on      │  DrainRequest(deadline_ms) ────────▶│   gate new turns NOW
    │  the send POST)  │◀─ DrainAck(in_flight = 42) ─────────┤   "busy — turn 42 running"
    │                  │                                     │
    │                  │   …waiting (≤ deadline_ms)…         │   turn 42 still computing
    │                  │                                     ●   turn 43 arrives → QUEUED
    │                  │                                     │   in-guest, not started
    │                  │◀─ TurnEnded(42) ────────────────────┤   app-quiescent at last
    │                  │  snapshot v, stream RAM+state ──▶ hostd@A     │
    │                  │  … move proceeds as before …        │
    │                  │                                  (on target, after Resumed:
    │                  │                                   guestd replays queued turn 43)
```

If the turn does **not** finish within `deadline_ms`, the move is abandoned, not forced:

```
    │                  │   …deadline_ms elapses, turn 42 still running…  │
    │                  │  DrainCancel ──────────────────────────────────▶│  un-gate;
    │                  │                                                  │  replay queue
    │◀── "drain timed out / aborted" (send fails) ─┤                      │
    │   VM stays on C. rebalancer logs it, moves on next /status poll.    │
```

So a single arrow in the walkthrough can quietly stall for up to `deadline_ms` while an
agent finishes a turn — and if it never goes idle, the VM simply stays put and the
rebalancer retries later. **No turn is ever interrupted to make a move happen.** This is
also exactly what the demo's `migrate-when-idle` waits for, just driven by hand instead
of by `pick_victim`.

The teaching points:

1. **Rebalancer ↔ hostd is the only control link.** Three `/status` GETs to decide,
   then exactly two POSTs to act: `recv` to the target, `send` to the source.
2. **Target before source.** `hostd@A` must be listening before `hostd@C` streams, or
   the snapshot has nowhere to land.
3. **Only the source hostd talks to the guest.** `hostd@C` sends `DrainRequest` and
   waits for `DrainAck` over its local channel to `guestd(v)`. The rebalancer is
   blocked on the `send` POST this whole time — it does not see the drain.
4. **The heavy transfer is hostd→hostd, direct.** RAM + state stream **C ⟶ A** on the
   data port; it never goes through the rebalancer.
5. **The guest changes which hostd it talks to.** Pre-move: `guestd(v)` ↔ `hostd@C`.
   The first thing it says on the target is `Resumed` to `hostd@A`. Same VM, same
   MAC/IP, new local daemon.
6. **`hostd@B` is only ever polled.** It was a candidate target; this step it lost to
   A, so it gets one `/status` GET and nothing more.

## What each move costs

Every arrow above is a full migration: drain to verified quiescence, snapshot, stream,
UFFD lazy-restore on the target, gratuitous ARP, resume. The
[race rule](../quiescence/race-rule.md) holds for each one — a VM mid-turn is never
interrupted, so a "move" may wait for an idle gap before it completes. Rebalancing a
hot fleet is therefore paced by quiescence, not by raw transfer speed.

> Status: the decision loop (`rebalance_once`, `pick_victim`) is implemented and unit-
> tested; the live `/proc`-sampled idle signal is still a fixed placeholder, so victim
> choice within a host is currently deterministic rather than idle-aware. Pre-v0.1.0.
