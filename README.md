# sleepwalk

Zero-perceived-downtime rebalancing for [Firecracker](https://firecracker-microvm.github.io/) microVMs: relocate a running VM between hosts by snapshotting it, transferring the memory, and lazily restoring it on the target via [userfaultfd](https://man7.org/linux/man-pages/man2/userfaultfd.2.html) — gated on *verified* workload quiescence, so the VM is paused during a real idle gap, moved, and wakes on another host none the wiser. Built for agent-sandbox and job-shaped workloads whose state is externalized and whose turns have natural pauses; no Firecracker fork, no kernel patches, Apache-2.0.

**Status:** pre-alpha, pre-`v0.1.0` — under active construction, nothing here is stable yet. See [`ROADMAP.md`](ROADMAP.md) once published, or run `just --list` for the current entry points.

## License

Apache-2.0.
