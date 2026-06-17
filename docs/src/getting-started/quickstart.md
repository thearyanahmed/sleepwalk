# Quickstart

> **Status:** pre-alpha, pre-`v0.1.0`. The `sleepwalk` CLI front door is still
> stubbed (`not_wired` — see [CLI & configuration](../operations/cli.md)); today the
> system is driven through `just` targets and the helper binaries. This page shows
> what runs *now*.

## Prerequisites

| Need | Why |
|------|-----|
| Linux with `/dev/kvm` | Firecracker requires KVM; KVM requires Linux. On macOS use a Linux VM — see [Development environment](environment.md). |
| Rust (pinned via `rust-toolchain.toml`) | Build the workspace. |
| `just` | The single entry point for every action. |
| Pinned Firecracker binary + guest kernel | Fetched and checksummed by `just fetch`. |

A lot runs **without** KVM — unit tests, the chaos race test, the UFFD page server
(needs Linux `userfaultfd` but not KVM), and the vsock transport test. Only the
VM-facing integration and benchmark targets need `/dev/kvm`.

## Tier 1 — runs anywhere (incl. macOS native)

```bash
just test          # unit + mock tests across the workspace
just lint          # cargo fmt --check + clippy -D warnings
just chaos         # turn-vs-drain race-rule chaos over seeded interleavings (no VM)
```

## Tier 1 (Linux, no KVM needed)

```bash
just uffd-test     # UFFD lazy-restore page server (needs Linux userfaultfd)
just vsock-test    # AF_VSOCK transport round-trip over loopback
```

## Get the artifacts

```bash
just fetch         # download + checksum the pinned Firecracker binary and kernel
just guest-rootfs  # build the synthetic guest rootfs (static guestd as init)
```

## Tier 2 — functional KVM (needs `/dev/kvm`)

```bash
just lifecycle-test   # single-host Firecracker boot lifecycle
just restore-test     # single-host snapshot -> UFFD lazy restore of a live VM
just migrate-test     # A->B migration with memory streamed over TCP (loopback)
```

A two-process A→B migration across hosts:

```bash
# On the target host (B): start the receiver first.
just migrate-recv 0.0.0.0:9000

# On the source host (A): point the sender at B's IP.
just migrate-send <B-ip>:9000
```

## Tier 3 — benchmark-valid (real KVM only, refuses TCG)

```bash
just migrate-bench    # freeze-window benchmark: boot once, ping-pong N migrations
just bench-restore    # O2 freeze-window vs RAM table  (not implemented yet)
just e2e              # full fleet scenario, O5         (not implemented yet)
```

See **[The `just` target map](just-targets.md)** for the full list grouped by tier,
and **[Demos](../operations/demos.md)** for the two end-to-end walkthroughs.
