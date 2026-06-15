# ADR-004: CPU/TSC compatibility classes are a hard fleet constraint

**Status:** accepted

## Context

sleepwalk relocates a running microVM by snapshotting its memory + vCPU state on
the source host and restoring it on the target. A Firecracker snapshot is **not**
portable to an arbitrary host. Two axes matter:

1. **CPU features (CPUID/MSRs).** A guest enumerates CPU features at boot. If it
   is restored on a host lacking a feature it saw, it can fault. Firecracker's
   CPU templates (`T2`, `T2S`, `T2CL`, `T2A`, …) mask the guest-visible CPU down
   to a common subset to make a snapshot portable across CPUs — but only within
   one vendor, and only down to features every host in the set has.

2. **TSC frequency.** The guest's timebase is the CPU's Time Stamp Counter,
   calibrated at boot to the host's TSC frequency. On restore Firecracker calls
   `KVM_SET_TSC_KHZ` to present the source's TSC rate to the guest; if the source
   and target frequencies differ by more than ~250 ppm, that needs **hardware TSC
   scaling** (`KVM_CAP_TSC_CONTROL`). If the target CPU lacks it, the restore
   fails — `Could not set TSC scaling within the snapshot: Invalid argument`.
   CPU templates do **not** address TSC frequency, and there is no Firecracker
   option to tolerate a mismatch or to rebase the paravirtual clock instead
   (tracked upstream, parked). Bypassing it would mean forking Firecracker, which
   ADR-006 rules out.

This is not theoretical. On a shared cloud, the **same instance size can be
placed on different physical CPUs**; resizing a VM can relocate it to a different
host generation. Observed in practice: two same-slug droplets reported TSC of
1995 MHz and 2100 MHz — a ~5% gap, far over the 250 ppm tolerance — and a
cross-host restore failed at load even though the snapshot transferred fine.

## Decision

Treat CPU/TSC compatibility as a **first-class placement constraint**, the same
pattern every production hypervisor uses (VMware EVC clusters, OpenStack Nova
aggregates, AWS SnapStart's "same hardware configuration").

- Each host computes a **compatibility class**: `(cpu_vendor, cpu_model,
  tsc_khz, kernel major.minor)`. It is exposed on `GET /status` and as a
  `sleepwalk_host_info{host,class}` metric.
- Two hosts are compatible iff: same vendor, same model, kernel matching at the
  major.minor tier, and TSC within 250 ppm (Firecracker's own tolerance).
- The **rebalancer only selects a migration target within the source's class.**
  A cross-class move is refused up front, turning an opaque restore-time `EINVAL`
  into a legible placement decision.
- Optionally, a per-class **CPU template** applied at boot widens a class by
  masking feature differences (must be set before the snapshot, since CPUID
  latches at boot).

## Consequences

- Migrations are reliable: a move is only attempted when restore can succeed.
- A heterogeneous fleet fragments into classes; the scheduler must be class-aware
  and bin-packs within each class. Fewer, broader classes (lower baseline
  templates) pack better at some peak-performance cost.
- Cross-vendor (Intel↔AMD) live relocation is never possible; such a move
  requires a cold restart, not a snapshot restore.
- Hosts that expose hardware TSC scaling widen a class (Firecracker can bridge
  the frequency gap there); preferring them is a future placement refinement.

## References

- Firecracker snapshot support and CPU templates documentation.
- The compatibility predicate and detector live in `proto::CompatClass` and
  `hostd`'s class detector; placement uses it in `rebalancer`.
