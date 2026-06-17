# Data flow

A single migration, end to end, as data moving between the three planes.

```
 rebalancer                hostd (A)              hostd (B)              guestd
     │                        │                      │                    │
     │ 1. decide: move VM ────▶                      │                    │
     │    A → B (in class)    │                      │                    │
     │                        │ 2. DrainRequest ─────┼───────────────────▶│  gate new turns
     │                        │ 3. DrainAck ◀────────┼────────────────────│  in_flight: None
     │                        │    (verify 3 layers) │                    │
     │                        │                      │ 4. bind receiver   │
     │                        │ 5. pause + snapshot  │                    │  ← FREEZE starts
     │                        │ 6. stream mem+state ─▶ (TCP, checksummed)  │
     │                        │                      │ 7. UFFD restore +   │
     │                        │                      │    resume (empty)   │  ← FREEZE ends
     │                        │                      │ 8. faults served ◀──┼──── page faults
     │                        │                      │ 9. gratuitous ARP   │
     │                        │ 10. teardown source  │                    │
     │ 11. update placement ◀─┼──────────────────────┤                    │
```

## Step by step

1. **Decide.** The rebalancer notices host A is hot, picks the most-idle VM, and
   confirms host B is in the same [compatibility class](../security/cpu-tsc.md).
2. **Drain.** `hostd` (A) sends `DrainRequest` to the guest, which gates new turns.
3. **Verify quiescence.** The guest replies `DrainAck { in_flight: None }`, and
   `hostd` confirms all three [quiescence layers](../quiescence/layers.md) are quiet.
   If a turn is in flight, the [race rule](../quiescence/race-rule.md) lets it win and
   the migration stands down.
4. **Bind receiver.** `hostd` (B) opens a TCP listener and, for a networked VM,
   prepares to re-plumb the tap.
5. **Snapshot.** `hostd` (A) pauses the VM and writes `mem.snap` + `state.snap`. The
   **freeze window** begins here.
6. **Transfer.** The snapshot files (plus `net.json` / `vsock.txt` metadata for a
   networked VM) stream to B over TCP, length-prefixed and CRC32-checksummed.
7. **Restore.** `hostd` (B) stands up the [UFFD page server](../migration/target-uffd.md),
   loads the snapshot with a UFFD memory backend, and resumes the VM with empty
   memory. The freeze window **ends** the moment the guest resumes.
8. **Fault pages lazily.** As the guest touches memory, the kernel traps each first
   touch and the page server copies that page from `mem.snap`. The freeze is therefore
   independent of guest RAM size.
9. **Announce.** `hostd` (B) sends a gratuitous ARP so every host on the overlay
   relearns the VM's MAC immediately; the guest keeps its MAC/IP, so client connections
   follow it.
10. **Teardown source.** `hostd` (A) kills the source Firecracker process and removes
    its work dir and snapshot files.
11. **Update placement.** The rebalancer records the VM as now living on B.

## What moves, and what does not

| Moves at migration time | Does not move (already shared/recreated) |
|-------------------------|------------------------------------------|
| Guest RAM (`mem.snap`) | The rootfs / workspace overlay — synced to backing storage beforehand. |
| vCPU + device state (`state.snap`) | The kernel and Firecracker binary — present on every host. |
| Network identity (`net.json`: tap name, MAC, IP) | Live TCP connections — by design there are none at quiescence. |

Only memory and machine state cross the wire. This is what keeps the transfer small
and the design fork-free.
