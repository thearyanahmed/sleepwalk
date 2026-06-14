# `hostd`

The per-host daemon. Runs Firecracker microVMs on one host: drives their
lifecycle, will serve their memory pages on restore (UFFD), and moves snapshots
between hosts. Internal crate.

## What's here (first slice)

| Module               | Contents |
|----------------------|----------|
| `firecracker`        | `FirecrackerApi` — the control port (`boot`/`pause`/`resume`/`shutdown` + `create_snapshot`/`load_snapshot`) every Firecracker effect goes through — and `Firecracker`, the production impl: its HTTP API over the per-VM unix socket. Endpoints/bodies match the v1.16.0 spec; unit-tested against a stub unix-socket server (no `/dev/kvm` needed). |
| `pseudo_firecracker` | `PseudoFirecracker` — a recording, fault-injecting stand-in implementing the same trait, so the lifecycle logic tests without a VM. |
| `vm`                 | `Vm` — the lifecycle orchestrator. Tracks `RunState` and rejects illegal operations (pause before boot, resume while running) as typed errors before any Firecracker call. |
| `quiesce`            | `QuiescenceDetector` — the layered O3 predicate: app (gated + idle) + infra (vCPU quiet N samples + queues) + storage (sync caught up). Quiescent only when all three agree; defaults to *not* quiescent. Layer inputs are fed from the edges; the logic is pure. |
| `drain`              | `DrainCoordinator` — the host half of the drain protocol. Folds the guest's `DrainAck`/turn signals off the wire plus locally-sampled infra/storage state into a `DrainVerdict` (`Quiescent` or `Busy { in_flight }`). A pure folder, so the decision tests without a clock or socket; the async recv/sample/deadline loop belongs to the executor. |
| `uffd` *(Linux)*     | `PageFaultServer` — the lazy-restore page server. Registers the guest memory region with `userfaultfd`, then serves each page fault from the snapshot file (`UFFDIO_COPY`) or a zero page for a hole, on a dedicated thread. Holds the crate's only `unsafe`. Linux-only (cfg'd out on macOS); tested without a VM by faulting an anonymous region. `just uffd-test`. |
| `process`            | `FcProcess` — spawns the `firecracker` binary on its own API socket, redirects the guest serial console to a log, waits for the socket before returning, and kills+reaps on drop. The plain-spawn path; jailer confinement is later. |
| `statedir`           | `VmDir` — the per-VM on-disk layout (`<base>/vms/<vm-id>/`), API socket + log paths, and the jailer chroot target. |
| `transfer`           | `send_files`/`recv_files` — stream snapshot files between hosts, framed and chunked with a per-file CRC32. Transport-agnostic (works over any stream), tested over an in-memory buffer with real temp files. |

## Design

- **Traits as ports.** Every external Firecracker effect is behind
  `FirecrackerApi`, so the lifecycle logic tests in milliseconds with no
  `/dev/kvm`. The real implementation (HTTP over the per-VM unix socket) slots in
  behind the same trait.
- **Runtime state, not typestate.** `Vm` enforces legal order with a `RunState`
  field rather than the type parameter the migration FSM uses — because hostd
  holds VMs in collections and drives them from message handlers, where a
  state-in-the-type value would be unwieldy.
- **Failures don't corrupt state.** A rejected/unreachable Firecracker call
  leaves `RunState` unchanged, so a failed `boot` keeps the VM `Created` and a
  retry is legal.

## Real Firecracker (needs `/dev/kvm`)

`FcProcess` + the `Firecracker` client boot a real microVM end to end: `just
lifecycle-test` (feature `real-vm`) spawns Firecracker, configures machine /
boot-source / rootfs, boots, asserts the guest reaches userspace, then
pauses/resumes/reaps it.

**Snapshot → UFFD lazy restore works** (`just restore-test`): boot → pause →
`create_snapshot`, then restore a fresh Firecracker with `mem_backend = Uffd`,
its guest memory served lazily by `UffdRestoreHandler` from the snapshot file —
the restored VM resumes and stays alive. This is the core of zero-downtime
relocation, proven on a single host.

Run both on a KVM host after `just fetch`. Still to come: jailer confinement and
the two-host migration. `shutdown` issues `SendCtrlAltDel` (x86 graceful
power-off); aarch64 process-reaping uses the `FcProcess` kill path.

## Testing it in isolation

```
cargo test -p hostd        # lifecycle + state-dir via PseudoFirecracker; the real client via a stub socket
```
