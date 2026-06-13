# Guest protocol & migration state machine

This is the **integration contract**. Anything that speaks it — the stock
`guestd`, or your own workload's harness — interoperates with `sleepwalk`
without depending on our source. The Rust types in the `proto` crate mirror this
document; where they ever disagree, this document is the spec.

**Version:** `v1-draft` (`proto::PROTOCOL_VERSION`). Pre-1.0 the wire shape may
change with a CHANGELOG entry and a version bump; it is frozen at the v0.1.0
release.

## Transport

Newline-delimited JSON over vsock. Each VM has its own vsock context id (CID);
the host listens on a fixed port. One JSON object per line, UTF-8. Messages are
**externally tagged**: a payload message is `{"<Variant>": { ...fields }}` and a
fieldless message is the bare string `"<Variant>"`.

```
{"Hello":{"vm_id":"7e57...","guestd_version":"0.1.0"}}
{"TurnStarted":{"turn_id":7,"ts":1700000000000000000}}
"Ping"
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
gated and replayed after resume on the target. The turn is never sacrificed to a
migration. Validated by the turn-vs-drain chaos test.

## Secrets

`Secrets` carries API keys to the guest at boot **only** over this vsock channel
— never baked into the rootfs image, never on the kernel cmdline (both are
world-readable from the host). Snapshots are RAM dumps and therefore contain any
secret in guest memory: snapshot dirs are `0700`, transferred only over
sleepwalk's own channel, and deleted at `Cleanup`. Use a dedicated,
spend-limited, revocable key. The production answer (a credentials broker that
keeps the key out of guest memory entirely) is ADR-005.
