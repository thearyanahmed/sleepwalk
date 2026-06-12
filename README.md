# sleepwalk

Zero-perceived-downtime rebalancing for [Firecracker](https://firecracker-microvm.github.io/) microVMs: relocate a running VM between hosts by snapshotting it, transferring the memory, and lazily restoring it on the target via [userfaultfd](https://man7.org/linux/man-pages/man2/userfaultfd.2.html) — gated on *verified* workload quiescence, so the VM is paused during a real idle gap, moved, and wakes on another host none the wiser. Built for agent-sandbox and job-shaped workloads whose state is externalized and whose turns have natural pauses; no Firecracker fork, no kernel patches, Apache-2.0.

**Status:** pre-alpha, pre-`v0.1.0` — under active construction, nothing here is stable yet. See [`ROADMAP.md`](ROADMAP.md) once published, or run `just --list` for the current entry points.

## Local development

Firecracker needs KVM, so development happens inside a Linux VM with `/dev/kvm`. On
Apple Silicon without hardware nested virtualization (M1/M2), the local dev VM runs
under QEMU's software CPU emulator (TCG), which boots the full stack correctly but
~10–30× slower — fine for development and correctness, **never valid for benchmarks**.
See [`docs/environment.md`](docs/environment.md) for the supported dev paths (native
KVM on M3+/x86/remote, TCG on M1/M2) and the rationale.

## License

Apache-2.0.
