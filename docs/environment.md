# Development environment

Firecracker requires **KVM**, which requires **Linux**. `sleepwalk` is therefore
developed and tested inside a Linux environment with `/dev/kvm` — on macOS that
means a Linux VM. This page explains the supported dev paths and, in particular,
why local development on Apple Silicon uses a *software-emulated* CPU (TCG) and
what that does and does not let you do.

## Dev paths

| Path | Hardware | Mechanism | Valid for |
|------|----------|-----------|-----------|
| **A** | Apple **M3/M4**, macOS 15+ | Lima VM (`vmType: vz`, `nestedVirtualization: true`) → ARM64 Ubuntu with hardware `/dev/kvm` | everything, incl. benchmarks |
| **A′** | Apple **M1/M2** | QEMU **TCG** with software-emulated EL2 → ARM64 Ubuntu with a *functional* `/dev/kvm` | dev & correctness only — **never benchmarks** |
| **B** | M1/M2 + remote ARM64 Linux box | Same scripts over ssh on real hardware | everything, incl. benchmarks |
| **C** | Any x86_64 Linux with KVM | Direct, native KVM | everything, incl. benchmarks |

`just dev-vm` boots the path-A′ VM; `just dev-vm-setup` provisions it. All artifact
versions (Firecracker, guest kernel, dev-VM image) are pinned and checksummed in
[`images/versions.toml`](../images/versions.toml).

## What TCG is, and why path A′ uses it

**TCG** (Tiny Code Generator) is QEMU's pure-software CPU emulator: it JIT-translates
guest instructions into host instructions at runtime, with no hardware
virtualization involved.

Firecracker needs **KVM**, and on ARM KVM needs **EL2** — the hardware hypervisor
exception level. Apple added hardware nested virtualization (a guest seeing its own
EL2) only on **M3 silicon with the macOS 15 API**; M1/M2 cannot provide a guest real
EL2 in hardware, so nested KVM is impossible there *via hardware*.

TCG sidesteps this by **emulating EL2 in software**:

```
qemu-system-aarch64 -machine virt,virtualization=on,gic-version=3 \
                    -cpu max -accel tcg
```

KVM then loads *inside* the QEMU guest, a functional `/dev/kvm` appears, and
Firecracker boots microVMs. Every guest instruction is interpreted/JITed on the host
CPU, so the whole stack runs at roughly **10–30× slower** than native.

**This is the rule that matters:** path A′ is for **development and correctness only**
— snapshot/restore, vsock, UFFD, the FSM, the drain race all *exercise* correctly.
Any **timing** number from TCG is meaningless and must never be published. The
benchmark `just` targets refuse to run when they detect TCG; functional targets print
a "TCG — timing not representative" banner instead. Benchmark-valid measurements
come from path A, B, or C (native KVM).

### Native KVM vs. TCG, at a glance

| | Hardware accel | Speed | Where |
|---|---|---|---|
| **KVM** | yes (real EL2) | near-native | M3+, bare-metal Linux, remote ARM box, x86 Linux |
| **TCG** | no (emulated EL2) | ~10–30× slower | anywhere QEMU runs — the M1/M2 local dev path |

## Path A′ smoke test (M1/M2)

Before anything builds on path A′, a one-time smoke test confirms the whole chain
works on this machine: TCG guest boots → `/dev/kvm` present → Firecracker boots a
microVM → snapshot/restore round-trips. The verdict and observed slowdown for this
machine are recorded below.

- **Host:** Apple M1 (4 cores allotted to the dev VM, 6 GiB RAM), macOS 26.4.1
- **Date:** 2026-06-12
- **Verdict: PASS.** The full chain works:
  1. QEMU TCG guest boots with software-emulated EL2; `/dev/kvm` present and r/w.
  2. Firecracker v1.16.0 boots a microVM (guest kernel 6.1.155) — reaches Linux
     userspace.
  3. Pause → **Full snapshot** (vmstate + 256 MiB mem file).
  4. **Restore into a fresh Firecracker process** (`snapshot/load`, `resume_vm=true`)
     — the restored VM responds to the API.
- **Slowdown:** not yet a precise factor — that needs a native-KVM baseline (path
  B/C), deferred. Qualitatively, boot-to-userspace is a few seconds of wall time,
  but CPU-heavy guest work (the apt index hooks `cnf-update-db` / `apt-check`,
  squashfs→ext4 conversion) runs minutes and is dominated by emulation `sys` time.
  Treat the matrix's "~10–30×" as the working estimate until measured on real KVM.

**TCG gotcha (recorded so it isn't rediscovered):** Ubuntu's apt post-invoke hooks
(`command-not-found` / `update-notifier` / `unattended-upgrades`) are single-threaded
index crunchers that peg the emulated vCPUs for 1000s+, causing RCU stalls, soft
lockups, and a first boot that never converges. The dev-VM cloud-init does no apt,
and `setup.sh` installs only missing tools after disabling those hooks — see the
comments there.

If the smoke test fails on a given machine, path A′ is struck for it and M1/M2
development moves to path B (remote ARM64 Linux).
