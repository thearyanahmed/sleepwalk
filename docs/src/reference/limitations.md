# Limitations

Stated plainly. These are design boundaries of `v0.1`, not bugs.

## By design

- **No true live migration.** sleepwalk moves a VM only during a verified idle gap, by
  pause → snapshot → restore. A workload that is busy every millisecond is not a fit —
  that needs pre-copy/post-copy live migration, which would require forking Firecracker
  (ruled out by ADR-006). See the [introduction](../introduction.md).
- **No live TCP connection migration.** Relocation happens at quiescence, when by
  construction there are no in-flight connections to preserve.
- **CPU-homogeneous host pools required.** A snapshot restores only on a host in the
  same [compatibility class](../security/cpu-tsc.md) — same CPU vendor/model, kernel
  tier, and TSC within ~250 ppm. Cross-vendor (Intel↔AMD) relocation is never possible;
  it needs a cold restart. See [ADR-004](../security/cpu-tsc.md).
- **Linux/KVM only.** Firecracker requires KVM, which requires Linux. macOS development
  runs inside a Linux VM — see [Development environment](../getting-started/environment.md).
- **Pre-1.0 API instability.** CLI, config, and the wire protocol may change with a
  CHANGELOG entry and a minor-version bump until `v0.1.0` freezes them.

## Known rough edges

- **A restored VM cannot yet be re-migrated over vsock.** Firecracker does not
  re-create the host-side vsock socket on snapshot load, so a restored VM is drained
  over the guest-network TCP channel instead. The vsock path is carried with the
  snapshot as groundwork for the fix — the "terminal restored VM" limitation. See
  [Networking](../operations/networking.md).
- **Post-restore clock fix-up is not yet actively wired** on every path. It has not
  bitten at small freeze windows; a long freeze could cause TLS/token skew. Tracked.
- **Clock and RNG after restore.** The guest clock freezes at snapshot and resyncs on
  `Resumed`; without the fix-up, TLS/token-expiry logic can misbehave. RNG state is
  duplicated across restores — irrelevant for a single migration, relevant for snapshot
  *forking* (noted for completeness).
- **The `sleepwalk` CLI handlers are stubbed** (`not_wired`) until the host runtime is
  fully wired; drive the system through `just` targets for now. See
  [CLI & configuration](../operations/cli.md).

## Out of scope for v0.1

- Sophisticated placement algorithms — the v0.1 rebalancer heuristic is deliberately
  simple and pluggable later.
- GPU workloads; x86↔ARM snapshot portability.
- Multi-tenant security hardening of sleepwalk itself — it trusts its operators; the
  security posture and threat notes are documented, the hardening is post-0.1.
