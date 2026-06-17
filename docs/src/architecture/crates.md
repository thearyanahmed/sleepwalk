# The crates

A Cargo workspace. **Internal crates are never prefixed with the project name** —
`proto`, not `sleepwalk-proto`. Only the `sleepwalk` binary crate is published to
crates.io; the integration contract is the versioned [protocol document](../protocol.md),
not a Rust dependency, so a non-Rust guest is a first-class citizen.

| Crate | Role |
|-------|------|
| [`proto`](#proto) | The public contract: vsock messages, host API types, the migration FSM. |
| [`guestd`](#guestd) | In-VM supervisor (PID 1): handshake, turn signals, drain gate, secret handoff. |
| [`hostd`](#hostd) | Per-host daemon: Firecracker lifecycle, UFFD page server, snapshot transfer, quiescence. |
| [`rebalancer`](#rebalancer) | Control plane: placement, pressure detection, migration FSM driver. |
| [`harness`](#harness) | Load generator + chaos harness. |
| [`cli`](#cli) | The `sleepwalk` binary — the only published crate. |

## proto

The contract every other crate depends on. It mirrors the
[protocol document](../protocol.md); where they ever disagree, the document is the
spec. Key pieces:

- **`vsock`** — the wire messages (`Hello`, `TurnStarted`/`TurnEnded`, `DrainRequest`/
  `DrainAck`/`DrainCancel`, `Secrets`, `RunTurn`, `Resumed`, `Ping`/`Pong`), internally
  tagged JSON.
- **`fsm`** — the migration state machine, encoded as a **typestate** so illegal
  transitions do not compile. See [the state machine](../migration/overview.md).
- **`ids`** — `VmId`, `HostId`, `TurnId`.
- **`CompatClass`** — the CPU/TSC compatibility predicate (see [ADR-004](../security/cpu-tsc.md)).
- `PROTOCOL_VERSION` (`v1-draft`), `GUEST_VSOCK_PORT`, `GUEST_DRAIN_TCP_PORT`.

## guestd

Runs as PID 1 (init) inside the microVM. Two adoption modes:

- **Wrap mode (zero code)** — supervises an arbitrary command and *infers* turn
  boundaries from its stdout markers. Drain is **passive**: the host waits until the
  child is between turns; new turns are not gated.
- **Native mode (exact boundaries)** — the workload speaks the vsock messages directly,
  enabling active gating: turns that arrive after a `DrainRequest` are queued in-guest
  and replayed after resume.

Internals include a `TurnTracker` (an `Arc` + atomics shared between the child-stdout
reader task and the drain responders), an await-secrets mode that defers the child
until the first handshake delivers `Secrets`, and a passive drain responder shared by
the vsock and TCP transports. It never snapshots mid-turn.

## hostd

The biggest crate; the host-side mechanism. Notable modules:

| Module | Responsibility |
|--------|----------------|
| `firecracker` | Drive the Firecracker API socket: configure, boot, pause, resume, snapshot, load. |
| `migrate` | Migration orchestration — the [source](../migration/source.md) and [target](../migration/target-uffd.md) halves. |
| `uffd` | The [userfaultfd page server](../migration/target-uffd.md) for lazy restore. |
| `transfer` | [Snapshot transfer](../migration/transfer.md) over TCP — length-prefixed, checksummed. |
| `quiesce` | The [layered quiescence detector](../quiescence/layers.md). |
| `drain` | The drain protocol / race-rule responder. |
| `net` | Tap device plumbing, the overlay bridge, gratuitous ARP on restore. |
| `compat` | CPU/TSC compatibility-class detection. |
| `registry` | The running-VM registry; boot-secret delivery. |
| `guestlink` / `guestload` | Talk to a guest over vsock or TCP; drive turns at a booted guest. |
| `telemetry` | `/metrics` + structured transcripts. |

## rebalancer

The control plane. Placement map, host memory-pressure signal (real or injected),
a pick-victim heuristic (most-idle VM on the hottest host), and the loop that drives
the migration FSM through its phases — filtering candidate targets to those in the
source's [compatibility class](../security/cpu-tsc.md).

## harness

An open-loop, rate-controlled load generator (latency measured from *intended* send
time, to avoid coordinated omission) and the chaos harness that validates the
[race rule](../quiescence/race-rule.md) over many seeded interleavings.

## cli

The `sleepwalk` binary — the front door. Subcommands: `host run`, `vm create|list|
status`, `migrate <vm> --to <host>`, `rebalance --watch`, `quiesce <vm>`. Parsing and
config loading are real; the handlers that need the host runtime are stubbed
(`not_wired`) until that runtime is fully wired. See
[CLI & configuration](../operations/cli.md).
