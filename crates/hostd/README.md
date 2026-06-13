# `hostd`

The per-host daemon. Runs Firecracker microVMs on one host: drives their
lifecycle, will serve their memory pages on restore (UFFD), and moves snapshots
between hosts. Internal crate.

## What's here (first slice)

| Module               | Contents |
|----------------------|----------|
| `firecracker`        | `FirecrackerApi` — the control port (`boot`/`pause`/`resume`/`shutdown`) every Firecracker effect goes through — and `Firecracker`, the production impl: its HTTP API over the per-VM unix socket. Endpoints/bodies match the v1.16.0 spec; unit-tested against a stub unix-socket server (no `/dev/kvm` needed). |
| `pseudo_firecracker` | `PseudoFirecracker` — a recording, fault-injecting stand-in implementing the same trait, so the lifecycle logic tests without a VM. |
| `vm`                 | `Vm` — the lifecycle orchestrator. Tracks `RunState` and rejects illegal operations (pause before boot, resume while running) as typed errors before any Firecracker call. |
| `statedir`           | `VmDir` — the per-VM on-disk layout (`<base>/vms/<vm-id>/`), API socket + log paths, and the jailer chroot target. |

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

## Not here yet (needs `/dev/kvm`)

`Firecracker` speaks the API, but jailer spawn + process teardown and the end-to-end
test against a *real* Firecracker require a Linux host with KVM. They land in a
later slice, exercised by `just lifecycle-test`. `shutdown` currently issues
`SendCtrlAltDel` (x86 graceful power-off); aarch64 process-reaping arrives with
the spawn slice.

## Testing it in isolation

```
cargo test -p hostd        # lifecycle + state-dir via PseudoFirecracker; the real client via a stub socket
```
