# Introduction

**sleepwalk** moves a running [Firecracker](https://firecracker-microvm.github.io/)
microVM from one physical host to another, and the workload inside never notices.
The VM is paused during a verified idle gap, its memory and CPU state are streamed
to the target host, and it wakes up on the other side none the wiser — hence the
name: the VM is asleep, gets moved, and keeps walking.

It is a Rust workspace, Apache-2.0 licensed, pre-`v0.1.0`.

## The problem

You run a fleet of microVMs across several hosts. One host gets **hot** — too much
memory pressure, too much CPU. You want to relocate a VM off it without losing the
work running inside. The classic options both hurt:

- **Kill and restart elsewhere** — the workload loses its in-memory state.
- **True live migration** (copy memory while the VM keeps running) — for Firecracker
  this requires a *fork of Firecracker plus kernel patches*. The one project that
  did it (Loophole Labs' Drafter/Silo) was AGPL-licensed and is now archived.

## The trade sleepwalk makes

Instead of "migrate anytime, hide the freeze even mid-request," sleepwalk says:

> **Only migrate in the idle gaps between turns.**

Agent-shaped and job-shaped workloads have constant natural pauses — a coding agent
waiting for your next prompt, a job between steps. Move the VM *during* one of those
pauses and the freeze never touches running work. The cost is that you cannot move a
VM mid-request; the payoff is **no Firecracker fork, no kernel patches, a permissive
license**, and a freeze that interrupts nothing.

This is the right trade for workloads whose state is already externalized and whose
turns have natural gaps. It is the wrong trade for a workload that is busy every
millisecond — that needs true live migration, which sleepwalk deliberately does not do.

## The five mechanisms

1. **Snapshot** — Firecracker pauses the VM and dumps RAM plus virtual-machine state
   (vmstate) to disk.
2. **Transfer** — stream that memory and state over the network (TCP), length-prefixed
   and checksummed.
3. **Lazy restore via userfaultfd (UFFD)** — on the target the VM resumes
   *immediately* with empty memory; the Linux
   [`userfaultfd`](https://man7.org/linux/man-pages/man2/userfaultfd.2.html) facility
   traps each first touch of a page and sleepwalk serves it on demand from the
   snapshot file. The freeze window is therefore **independent of guest RAM size**.
4. **Verified quiescence** — never *assume* the VM is idle; *prove* it. Three layers
   (app, infra, storage) must all agree before any move fires.
5. **The race rule** — if a turn races a migration, the outcome is deterministic:
   **in-flight turn > migration > queued turn**. A running turn is never sacrificed.

## Prior art and positioning

| Project | Live migration | License | Firecracker fork? | Status |
|---------|----------------|---------|-------------------|--------|
| Drafter / Silo (Loophole Labs) | yes (true) | AGPL-3.0 | yes + kernel patches | archived |
| Cloud Hypervisor | yes (hand-coordinated primitive) | Apache-2.0 | n/a (not Firecracker) | active |
| **sleepwalk** | no — moves only in idle gaps | Apache-2.0 | **no fork, no patches** | pre-`v0.1.0` |

sleepwalk deliberately occupies the gap: it trades "migrate anytime" for "migrate
only in the idle gaps between turns, so the freeze never touches a running turn,"
which is the right trade for agent/job-shaped workloads whose state is externalized
anyway.

## How to read this book

- New here? Start with **[System overview](architecture/overview.md)**.
- Want the mechanism? **[The migration pipeline](migration/overview.md)**.
- Want the safety argument? **[Quiescence & the race rule](quiescence/layers.md)**.
- Integrating your own workload? **[Guest protocol](protocol.md)** — the contract.
- Unsure of a term? **[Glossary](reference/glossary.md)**.
