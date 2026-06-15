# Guest protocol & migration state machine

This is the **integration contract**. Anything that speaks it — the stock
`guestd`, or your own workload's harness — interoperates with `sleepwalk`
without depending on our source. The Rust types in the `proto` crate mirror this
document; where they ever disagree, this document is the spec.

**Version:** `v1-draft` (`proto::PROTOCOL_VERSION`). Pre-1.0 the wire shape may
change with a CHANGELOG entry and a version bump; it is frozen at the v0.1.0
release.

## Adoption modes

There are two ways to put a workload under `sleepwalk`, both producing the same
wire messages below:

- **Wrap mode (zero code).** The stock `guestd` supervises an arbitrary command
  and *infers* turn boundaries from its **stdout**: a line equal to a configured
  start marker opens a turn (`TurnStarted`), a line equal to the end marker
  closes it (`TurnEnded`). Set `SLEEPWALK_WRAP_CMD` to the command — or, for a
  baked rootfs with no shell, drop it in `/etc/sleepwalk/wrap-cmd`. The command
  is exec'd directly (argv split on whitespace; the minimal guest has no shell).
  `SLEEPWALK_WRAP_START` / `SLEEPWALK_WRAP_END` override the default markers
  (`@@TURN_START@@` / `@@TURN_END@@`). Any other output is passed
  through to guestd's log. Good for job-shaped workloads that can print a line at
  the edges of a unit of work. The wrapped process keeps running across a
  migration — its in-RAM state is carried in the snapshot — so it must not assume
  a stable host clock or local-only network state across a turn boundary.

  Wrap mode only *observes*: guestd cannot defer a turn the child has already
  begun. Drain is therefore **passive** — the host waits until the child is
  between turns (no turn in flight) before snapshotting. New turns are not gated
  or queued; that is the native-mode guarantee.

- **Native mode (exact boundaries).** The workload (or its harness) speaks the
  vsock messages directly — `TurnStarted` / `TurnEnded` / `DrainAck` — for exact
  turn boundaries and active gating: new turns that arrive after a `DrainRequest`
  are queued in-guest and replayed after resume (the race rule below). This is
  what an agent/turn-shaped integration uses.

## Transport

Newline-delimited JSON over vsock. Each VM has its own vsock context id (CID);
the host listens on a fixed port. One JSON object per line, UTF-8. Messages are
**internally tagged**: every message is a flat object with a `type` field naming
the message, plus that message's fields. A fieldless message is just `{"type":"<Name>"}`.

```
{"type":"Hello","vm_id":"7e57...","guestd_version":"0.1.0"}
{"type":"TurnStarted","turn_id":7,"ts":1700000000000000000}
{"type":"Ping"}
```

## Messages

Direction is fixed per message; there is no message a guest may send to itself
or a host may send in the wrong direction.

### Guest → Host

| Message | Fields | Meaning |
|---------|--------|---------|
| `Hello` | `vm_id` (UUID string), `guestd_version` (string) | Boot handshake; binds the connection to a VM and declares the guestd version. Must be first. |
| `TurnStarted` | `turn_id` (int), `ts` (int ns) | A unit of guest work began. The VM is now non-quiescent at the app layer. |
| `TurnEnded` | `turn_id` (int), `ts` (int ns) | That turn finished. |
| `DrainAck` | `in_flight` (int turn id **or** `null`) | Reply to `DrainRequest`. `null` ⇒ new turns gated **and** none running (app-layer quiescent). A turn id ⇒ wait for it (or time out). The `null` is always present, never omitted. |
| `Resumed` | `ts` (int ns) | First message after a restore on the target host. Triggers guest clock fix-up. |
| `Ping` / `Pong` | — | Liveness. |

### Host → Guest

| Message | Fields | Meaning |
|---------|--------|---------|
| `Secrets` | `env` (object: string → string) | API keys / secrets injected at boot. Never in the rootfs or kernel cmdline — see Secrets below. |
| `DrainRequest` | `deadline_ms` (int) | Gate new turns and report what's in flight. `deadline_ms` is how long the host will wait for an in-flight turn before aborting the migration. |
| `DrainCancel` | — | Migration aborted; un-gate and release any queued turns. |
| `Ping` / `Pong` | — | Liveness. |

### Field encodings

- **Durations** cross the wire as whole **milliseconds** (`deadline_ms`). The
  Rust side holds a `Duration`; the `_ms` suffix is the unit on the wire.
- **Timestamps** (`ts`) are integer **nanoseconds since the Unix epoch**, as the
  *guest* observed them. The guest clock freezes at snapshot and resyncs on
  `Resumed`; timestamps that straddle a migration are not comparable until then.
- **`turn_id`** is a monotonic per-VM counter starting at 0. Absence of an
  in-flight turn is `null`, never turn `0`.

## Migration state machine

Rebalancer-owned. The `proto` crate encodes the legal transitions as a
typestate, so e.g. snapshotting before quiescence does not compile.

```
Stable ─▶ Intent ─▶ Draining ─▶ Quiescent ─▶ Snapshotting ─▶ Transferring
  ▲          │          │                                          │
  │          └─ abort ──┴── (timeout / turn-in-flight too long)    │
  │                                                                ▼
Cleanup ◀── CutOver ◀── Restoring ◀────────────────────────────────┘
```

- **Abort** is legal from any phase **before** `Snapshotting` (`Intent`,
  `Draining`, `Quiescent`) and returns the VM to `Stable` on the source host.
  Once `Snapshotting` begins, the migration runs to completion or fails over to
  resume-on-source — there is no abort, and the type system enforces it.
- Every transition is emitted as a structured JSON event into the run transcript
  (`results/`) and surfaced on the rebalancer's `/metrics` as an FSM gauge.

## The race rule (normative)

Precedence: **in-flight turn > migration > queued turn.** A turn already running
when a `DrainRequest` arrives wins — the migration waits up to `deadline_ms`,
then `DrainCancel`s back to `Stable`. New turns that arrive after the drain are
**queued in-guest**, not dropped: each is held as backlog and **replayed** once
the gate reopens — after `Resumed` on the target host (the common path) or after
a `DrainCancel` un-gates on the source (abort path). "Zero dropped turns" is
exactly this: a gated turn is deferred, never lost. The turn is never sacrificed
to a migration. A turn-start that races the drain in the same instant resolves by
the guest's local processing order: if `TurnStarted` was emitted before
`DrainRequest` was handled, it counts as in-flight and wins.

Validated by the turn-vs-drain chaos test: a drain dropped at random offsets
across a stream of turns, over many seeded interleavings, asserting (1) every
attempted turn eventually runs (zero dropped), (2) the `DrainAck`'s `in_flight`
matches the turn actually running at the drain instant, and (3) no turn starts
while the gate is closed. The KVM wall-clock version (100 runs on `/dev/kvm`)
is the integration-tier counterpart.

## Secrets

`Secrets` carries API keys to the guest at boot **only** over this vsock channel
— never baked into the rootfs image, never on the kernel cmdline (both are
world-readable from the host). Snapshots are RAM dumps and therefore contain any
secret in guest memory: snapshot dirs are `0700`, transferred only over
sleepwalk's own channel, and deleted at `Cleanup`. Use a dedicated,
spend-limited, revocable key. The production answer (a credentials broker that
keeps the key out of guest memory entirely) is ADR-005.
