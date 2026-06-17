# Source side

The source half lives in `hostd::migrate::migrate_running` (the daemon path for an
already-running VM) and `migrate_source` (the benchmark path that boots a fresh VM
first). Both converge on `snapshot_and_send`, which is where the freeze window lives.

## 1. Drain to quiescence — over TCP, not vsock

```rust
let drained = if let Some(net) = &vm.net {
    let addr = format!("{}:{}", net.ip, proto::GUEST_DRAIN_TCP_PORT);
    match GuestLink::connect_tcp_retry(&addr, secs(20)).await {
        Ok(link) => drain_to_quiescence(&link).await,
        Err(e) => return Ok(MigrateOutcome::StoodDown(vm)), // VM kept alive
    }
} else {
    // non-networked VM: vsock, one-way (it can't re-migrate anyway)
    ...
};
```

A **networked** VM is drained over the guest **network (TCP)**, not vsock. The reason
is structural: *Firecracker stops servicing vsock connections after a snapshot
restore* (both directions), but the guest network survives a restore. So TCP is the
only channel that can drain — and therefore re-migrate — a VM that has *already* been
moved once. A non-networked VM falls back to vsock and is effectively one-way.

## 2. The safety gate

`drain_to_quiescence` does handshake → `link.drain(deadline)`:

```rust
async fn drain_to_quiescence(link) -> io::Result<DrainState> {
    link.handshake(BTreeMap::new()).await?;
    let state = link.drain(DRAIN_DEADLINE).await?;   // DRAIN_DEADLINE = 5s
    if state == DrainState::Busy {
        let _ = link.send(proto::HostToGuest::DrainCancel).await; // reopen the gate
    }
    Ok(state)
}
```

The outcomes:

| Drain result | What happens | Why |
|--------------|--------------|-----|
| `Quiescent` | Proceed to snapshot. | Verified idle. |
| `Busy` | Send `DrainCancel`, return `StoodDown(vm)` — VM handed back **intact**. | The [race rule](../quiescence/race-rule.md): an in-flight turn beats the migration. |
| connect/drain error | Also `StoodDown` (or stand down) — VM kept alive. | **Pre-snapshot failures never destroy the VM.** |

This is the race rule expressed as control flow: nothing is snapshotted until the
guest has proven it is between turns.

## 3. Pack network identity

A networked VM carries metadata files alongside the snapshot so the target can
reconstruct its identity:

- **`net.json`** — the tap device name, MAC, and IP. The target re-creates the *same*
  tap before loading, so the guest keeps its MAC/IP and client connections follow it.
- **`vsock.txt`** — the guest's vsock UDS path. Carried as groundwork for re-migrating
  a restored VM (see the [limitation](../reference/limitations.md)).

## 4. Snapshot + send — the freeze window

```rust
let t0 = Instant::now();
fc.pause().await?;                                    // ← VM frozen here
fc.create_snapshot(SnapshotTarget { mem_file, state_file }).await?;
let t1 = Instant::now();
send_snapshot(addr, &files).await?;                  // stream over TCP
let t2 = Instant::now();

SourceTiming {
    snapshot: t1 - t0,   // pause → snapshot written
    transfer: t2 - t1,   // snapshot written → transfer complete
    bytes,               // memory snapshot size
}
```

`mem.snap` (guest RAM) and `state.snap` (vCPU + device state) are written, then
streamed with any extra metadata files. See [Snapshot transfer](transfer.md) for the
wire format.

## 5. Teardown and telemetry

On success the source VM is torn down (`teardown()`), and the freeze window plus bytes
moved are recorded to telemetry (`telemetry::migration_ok`). On a snapshot/transfer
error the failure is recorded (`telemetry::migration_failed`) and the VM is still torn
down — but recall that *errors before* `Snapshotting` stand the VM down intact instead.

## The two outcomes

```rust
pub enum MigrateOutcome {
    Moved(SourceTiming),   // drained, snapshotted, streamed, torn down — with timing
    StoodDown(RunningVm),  // guest was busy; migration stood down; VM returned intact
}
```

`StoodDown` is not an error — it is the race rule working. The caller re-registers the
VM and tries again at the next idle gap.
