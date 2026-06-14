# sleepwalk

Zero-perceived-downtime rebalancing for [Firecracker](https://firecracker-microvm.github.io/) microVMs: relocate a running VM between hosts by snapshotting it, transferring the memory, and lazily restoring it on the target via [userfaultfd](https://man7.org/linux/man-pages/man2/userfaultfd.2.html) — gated on *verified* workload quiescence, so the VM is paused during a real idle gap, moved, and wakes on another host none the wiser. Built for agent-sandbox and job-shaped workloads whose state is externalized and whose turns have natural pauses; no Firecracker fork, no kernel patches, Apache-2.0.

**Status:** pre-alpha, pre-`v0.1.0` — under active construction, nothing here is stable yet. Run `just --list` for the current entry points.

## Local development

Firecracker needs KVM, so development happens inside a Linux VM with `/dev/kvm`. On
Apple Silicon without hardware nested virtualization (M1/M2), the local dev VM runs
under QEMU's software CPU emulator (TCG), which boots the full stack correctly but
~10–30× slower — fine for development and correctness, **never valid for benchmarks**.
See [`docs/environment.md`](docs/environment.md) for the supported dev paths (native
KVM on M3+/x86/remote, TCG on M1/M2) and the rationale.

## Preliminary measurement (single-host)

First freeze-window numbers from `just migrate-bench` — boot one microVM, then
migrate it 20 times (snapshot → UFFD lazy restore → resume), timing only the
window the guest is paused.

| metric | value |
|--------|------:|
| migrations | 20 |
| min freeze | 356.8 ms |
| max freeze | 1457.8 ms |
| mean freeze | 1183.0 ms |
| guest RAM | 256 MB |
| memory moved / migration | 256 MB |

**Methodology:** Firecracker v1.16.0, guest kernel 6.1.155, 1 vCPU / 256 MB
guest, single host (snapshot files on local disk — **no network transfer**), 20
ping-pong cycles with a 1 s settle between each, snapshot mem file under a
disk-backed `/tmp`.

**Read these honestly — they are not a headline result:**

- **Not zero-downtime.** ~1.2 s is dominated by `create_snapshot` writing the
  full 256 MB RAM dump to disk *inside* the paused window. userfaultfd speeds the
  *restore* side (pages fault in lazily after resume); it does not speed the
  snapshot. Cutting the freeze means diff snapshots (dirty pages only) and/or
  placing the mem file on tmpfs — future work.
- **Not benchmark-valid.** 1 vCPU and a disk-backed `/tmp` on a shared box; per
  the [environment matrix](docs/environment.md) such numbers are never published
  as results.
- **Single host only.** No memory is sent over a network here; the real
  freeze-window + relocation numbers come from a two-droplet A→B migration on a
  CPU-homogeneous, multi-core pair — still ahead.

The point of this run is that the instrument exists and the core path works: a
running VM is snapshotted and lazily restored via the project's own UFFD page
server and keeps running. The number is a starting baseline, not the claim.

## License

Apache-2.0.
