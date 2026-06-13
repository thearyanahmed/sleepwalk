# `hostd`

The per-host daemon. Runs Firecracker microVMs on one host: drives their
lifecycle, will serve their memory pages on restore (UFFD), and moves snapshots
between hosts. Internal crate.

## What's here (first slice)

| Module     | Contents |
|------------|----------|
| `fc`       | `FirecrackerApi` — the control port (`boot`/`pause`/`resume`/`shutdown`) every Firecracker effect goes through — plus `FakeFc`, a recording, fault-injecting fake for tests. |
| `vm`       | `Vm` — the lifecycle orchestrator. Tracks `RunState` and rejects illegal operations (pause before boot, resume while running) as typed errors before any Firecracker call. |
| `statedir` | `VmDir` — the per-VM on-disk layout (`<base>/vms/<vm-id>/`), API socket + log paths, and the jailer chroot target. |

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

The real `FirecrackerApi` implementation and jailer spawn require a Linux host
with KVM (the dev VM). They land in a later slice, exercised by
`just lifecycle-test`.

## Testing it in isolation

```
cargo test -p hostd        # lifecycle orchestration + state-dir layout, all via FakeFc
```
