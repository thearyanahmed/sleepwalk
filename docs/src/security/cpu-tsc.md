# CPU/TSC compatibility (ADR-004)

A Firecracker snapshot is **not** portable to an arbitrary host. The two axes that
matter are CPU features (CPUID/MSRs) and **TSC frequency** — and a mismatch on either
makes a restore fail, even after the snapshot transferred perfectly. sleepwalk treats
compatibility as a first-class placement constraint: the rebalancer only selects a
migration target within the source's **compatibility class**, turning an opaque
restore-time `EINVAL` into a legible up-front placement decision.

The full decision record follows.

---

{{#include ../../adr/0004-cpu-tsc-compatibility-classes.md}}
