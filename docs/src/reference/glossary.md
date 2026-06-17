# Glossary

Acronyms and terms, full form first.

| Term | Full form / meaning |
|------|---------------------|
| **Firecracker** | Amazon's lightweight virtual-machine monitor (VMM) for microVMs; powers AWS Lambda and Fargate. sleepwalk drives it unmodified — no fork, no kernel patches. |
| **microVM** | A minimal virtual machine (one or a few vCPUs, small RAM, paravirtual devices) booted by Firecracker in tens of milliseconds. |
| **VMM** | Virtual Machine Monitor — the userspace process that creates and controls a VM (Firecracker, here). |
| **KVM** | Kernel-based Virtual Machine — the Linux kernel's hardware-virtualization interface (`/dev/kvm`). Firecracker requires it; KVM requires Linux. |
| **UFFD** | userfaultfd(2) — a Linux facility that lets a userspace process handle page faults for a memory region. sleepwalk uses it for [lazy restore](../migration/target-uffd.md). |
| **lazy restore** | Resuming a VM with empty memory and faulting each page in on first touch, so the freeze window is independent of guest RAM size. |
| **page fault** | A CPU trap when a thread touches a memory page not currently backed; with UFFD, sleepwalk serves the page from the snapshot file. |
| **vsock** | Virtual socket — a host↔guest socket transport (AF_VSOCK). The boot/turn channel between `guestd` and `hostd`. Stops working after a restore. |
| **CID** | Context ID — a vsock address; each VM has its own. The host is always CID 2. |
| **snapshot** | Firecracker's capture of a paused VM: a memory file (`mem.snap`) + a VM-state file (`state.snap`). A RAM dump — treat as secret-bearing. |
| **vmstate** | The non-memory machine state in a snapshot: vCPU registers, device state, clock. |
| **FSM** | Finite State Machine — a model with a fixed set of states and only legal transitions between them. The [migration FSM](../migration/overview.md) is encoded as a Rust typestate. |
| **typestate** | A Rust pattern where a value's state is a type parameter, so illegal operations fail to **compile** rather than at runtime. |
| **quiescence** | A verified-idle state of a VM: no work in flight. sleepwalk requires three [layers](../quiescence/layers.md) to agree before migrating. |
| **quiescent** | Adjective: at rest, inactive — the verified-idle condition that lets a migration proceed. |
| **drain** | The protocol step that gates new turns and waits for in-flight work to finish, reaching quiescence. |
| **the race rule** | The normative precedence **in-flight turn > migration > queued turn**; a running turn is never sacrificed. See [the race rule](../quiescence/race-rule.md). |
| **turn** | One unit of guest work (an agent message, a job step). The granularity at which migrations are allowed to land — *between* turns. |
| **wrap mode** | Zero-code adoption: `guestd` supervises an arbitrary command and infers turns from its stdout markers. Drain is passive. |
| **native mode** | The workload speaks the vsock protocol directly for exact turn boundaries and active gating. |
| **rebalancer** | The control plane: watches pressure, picks victims, drives the migration FSM. |
| **hostd** | The per-host daemon: Firecracker lifecycle, UFFD page server, snapshot transfer, quiescence. |
| **guestd** | The in-VM supervisor (PID 1): protocol, turn signals, drain gate, secret handoff. |
| **tap device** | A virtual L2 network interface on the host backing the guest's NIC; re-plumbed under the same name on the target so the guest keeps its MAC/IP. |
| **gratuitous ARP** | An unsolicited ARP broadcast announcing a MAC↔IP binding; sent after restore so the network relearns the VM's new location immediately. |
| **TSC** | Time Stamp Counter — a CPU register counting cycles; the guest's timebase. A frequency mismatch across hosts breaks restore — see [ADR-004](../security/cpu-tsc.md). |
| **CPUID** | The x86 instruction enumerating CPU features; latched by the guest at boot, which is why CPU templates must be applied before snapshot. |
| **compatibility class** | `(cpu_vendor, cpu_model, tsc_khz, kernel tier)`; the rebalancer only migrates within one. See [ADR-004](../security/cpu-tsc.md). |
| **clock fix-up** | Resyncing the guest clock on `Resumed`, because the guest clock froze at snapshot time; without it TLS/token-expiry logic can misbehave. See [Post-restore clock fix-up](../migration/clock-fixup.md). |
| **coordinated omission** | A load-test measurement bug where a closed-loop generator stops sending during a stall and hides the latency spike. The [harness](../architecture/crates.md#harness) is open-loop to avoid it. |
| **TCG** | Tiny Code Generator — QEMU's pure-software CPU emulator, used on the M1/M2 dev path. Correct but ~10–30× slow; never benchmark-valid. |
| **ADR** | Architecture Decision Record — a short doc capturing one decision, its context, and consequences (see `docs/adr/`). |
| **PoC** | Proof of Concept. |
