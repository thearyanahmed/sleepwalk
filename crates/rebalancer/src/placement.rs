//! Placement and the pick-victim heuristic.
//!
//! The rebalancer's decision half: given where VMs currently live
//! ([`Placement`]), how loaded each host is ([`Pressure`]), and how idle each VM
//! is, [`pick_victim`] chooses the single migration that best relieves pressure
//! — the **most-idle VM on the hottest host**, moved to the coolest host that
//! can take it. The driver half ([`crate::driver::drive`]) then carries that
//! choice through the migration FSM.
//!
//! The heuristic is intentionally one move at a time: pick the worst hotspot,
//! relieve it with the safest (most-idle, least likely to be cut by the race
//! rule) victim, re-measure, repeat. Multi-VM batch drains are a post-v0.1
//! candidate. Every input is fed from the edges (real `/proc` pressure, real
//! idle-gap tracking); the logic here is pure and total — it returns `None`
//! rather than guessing when no move helps.

use std::collections::BTreeMap;
use std::time::Duration;

use proto::{CompatClass, HostId, VmId};

/// A normalized memory-pressure reading for a host: `0.0` idle … `1.0`
/// saturated. Non-finite inputs collapse to `0.0` (absence of evidence is not
/// pressure), and values are clamped to `[0, 1]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pressure(f64);

impl Pressure {
    /// Build a pressure reading, clamping to `[0, 1]` and mapping non-finite
    /// values to `0.0`.
    #[must_use]
    pub fn new(value: f64) -> Self {
        if value.is_finite() {
            Self(value.clamp(0.0, 1.0))
        } else {
            Self(0.0)
        }
    }

    /// The clamped reading.
    #[must_use]
    pub fn get(self) -> f64 {
        self.0
    }
}

/// Which VMs currently live on which host.
#[derive(Debug, Clone, Default)]
pub struct Placement {
    hosts: BTreeMap<HostId, Vec<VmId>>,
}

impl Placement {
    /// An empty placement.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `vm` lives on `host`.
    pub fn assign(&mut self, host: HostId, vm: VmId) {
        self.hosts.entry(host).or_default().push(vm);
    }

    /// The VMs on `host` (empty if the host is unknown or bare).
    #[must_use]
    pub fn vms_on(&self, host: &HostId) -> &[VmId] {
        self.hosts.get(host).map_or(&[], Vec::as_slice)
    }

    /// The hosts in the placement, ascending by id.
    pub fn hosts(&self) -> impl Iterator<Item = &HostId> {
        self.hosts.keys()
    }
}

/// One rebalancing move: relocate `vm` from `from` to `to`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rebalance {
    /// The VM to migrate.
    pub vm: VmId,
    /// The overloaded source host.
    pub from: HostId,
    /// The chosen, cooler target host.
    pub to: HostId,
}

/// Choose the single best migration to relieve memory pressure, or `None` if no
/// move helps.
///
/// The rule, in order:
/// 1. Find the **hottest** host. If its pressure is below `high_watermark`,
///    nothing is overloaded — return `None`.
/// 2. Find the **coolest** other host **whose CPU/TSC class can accept the
///    snapshot** ([`CompatClass::compatible_with`]). A cooler but incompatible
///    host is skipped — restoring there would fail at load (the TSC-scaling
///    `EINVAL`), so it is not a valid target. If the source host has no class, or
///    no compatible host is strictly cooler, return `None`.
/// 3. On the hottest host, pick the **most-idle** VM (longest current idle gap):
///    the safest victim, least likely to be running a turn the race rule would
///    protect. A VM with no idleness reading is treated as fully busy (idle
///    `0`), never chosen on missing evidence. If the hottest host has no VMs,
///    return `None`.
///
/// Ties (equal pressure or equal idleness) break by `HostId`/`VmId` order, so
/// the choice is deterministic.
#[must_use]
pub fn pick_victim(
    placement: &Placement,
    pressure: &BTreeMap<HostId, Pressure>,
    idle: &BTreeMap<VmId, Duration>,
    high_watermark: Pressure,
    compat: &BTreeMap<HostId, CompatClass>,
) -> Option<Rebalance> {
    let hottest = hottest_host(pressure)?;
    if pressure_of(pressure, hottest) <= high_watermark.get() {
        return None; // nothing is overloaded
    }

    // The snapshot is taken on the source; only a host its class is compatible
    // with can restore it. No class for the source ⇒ we cannot place safely.
    let source_class = compat.get(hottest)?;
    let coolest = coolest_compatible_host(pressure, hottest, source_class, compat)?;
    if pressure_of(pressure, coolest) >= pressure_of(pressure, hottest) {
        return None; // no compatible host is cooler — a move would not relieve pressure
    }

    let vm = most_idle_vm(placement.vms_on(hottest), idle)?;

    Some(Rebalance {
        vm,
        from: hottest.clone(),
        to: coolest.clone(),
    })
}

/// The host with the greatest pressure (ties by `HostId` order).
fn hottest_host(pressure: &BTreeMap<HostId, Pressure>) -> Option<&HostId> {
    pressure
        .iter()
        .max_by(|(ha, pa), (hb, pb)| {
            pa.get()
                .total_cmp(&pb.get())
                // Lower id wins a tie: reverse so the smaller id is "max".
                .then(hb.cmp(ha))
        })
        .map(|(h, _)| h)
}

/// The coolest host other than `except` that is **compatible** with
/// `source_class` (can restore its snapshot). Ties by `HostId` order. Hosts with
/// no known class are skipped — we never migrate to a host we cannot vet.
fn coolest_compatible_host<'a>(
    pressure: &'a BTreeMap<HostId, Pressure>,
    except: &HostId,
    source_class: &CompatClass,
    compat: &BTreeMap<HostId, CompatClass>,
) -> Option<&'a HostId> {
    pressure
        .iter()
        .filter(|(h, _)| *h != except)
        .filter(|(h, _)| {
            compat
                .get(*h)
                .is_some_and(|target| source_class.compatible_with(target))
        })
        .min_by(|(ha, pa), (hb, pb)| pa.get().total_cmp(&pb.get()).then(ha.cmp(hb)))
        .map(|(h, _)| h)
}

/// The most-idle VM among `vms` (longest idle gap; ties by `VmId` order). A VM
/// with no reading counts as idle `0` — fully busy — so it is never chosen on
/// missing evidence.
fn most_idle_vm(vms: &[VmId], idle: &BTreeMap<VmId, Duration>) -> Option<VmId> {
    vms.iter().copied().max_by(|a, b| {
        let ia = idle.get(a).copied().unwrap_or_default();
        let ib = idle.get(b).copied().unwrap_or_default();
        // Lower id wins a tie.
        ia.cmp(&ib).then(b.cmp(a))
    })
}

fn pressure_of(pressure: &BTreeMap<HostId, Pressure>, host: &HostId) -> f64 {
    pressure.get(host).map_or(0.0, |p| p.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str) -> HostId {
        HostId::new(name)
    }

    fn vm() -> VmId {
        VmId::new()
    }

    fn klass(tsc_khz: u32) -> CompatClass {
        CompatClass {
            vendor: "GenuineIntel".to_owned(),
            model: "Xeon".to_owned(),
            tsc_khz,
            kernel: "6.1.155".to_owned(),
        }
    }

    /// All of `a`, `b`, `c` in one compatible class — the common test case.
    fn compat() -> BTreeMap<HostId, CompatClass> {
        ["a", "b", "c"]
            .into_iter()
            .map(|h| (host(h), klass(2_000_000)))
            .collect()
    }

    /// A two-host placement where host-a is hot and host-b is cool: the most-idle
    /// VM on host-a is chosen and moved to host-b.
    #[test]
    fn picks_most_idle_vm_on_hottest_host() {
        let busy = vm();
        let idle_vm = vm();
        let mut placement = Placement::new();
        placement.assign(host("a"), busy);
        placement.assign(host("a"), idle_vm);
        placement.assign(host("b"), vm());

        let pressure = BTreeMap::from([
            (host("a"), Pressure::new(0.95)),
            (host("b"), Pressure::new(0.20)),
        ]);
        let idle = BTreeMap::from([
            (busy, Duration::from_secs(1)),
            (idle_vm, Duration::from_secs(60)),
        ]);

        let pick = pick_victim(&placement, &pressure, &idle, Pressure::new(0.80), &compat())
            .expect("a move should be chosen");
        assert_eq!(
            pick,
            Rebalance {
                vm: idle_vm,
                from: host("a"),
                to: host("b"),
            }
        );
    }

    /// No host above the watermark — the fleet is fine, do nothing.
    #[test]
    fn no_move_when_nothing_is_overloaded() {
        let v = vm();
        let mut placement = Placement::new();
        placement.assign(host("a"), v);
        let pressure = BTreeMap::from([
            (host("a"), Pressure::new(0.50)),
            (host("b"), Pressure::new(0.10)),
        ]);
        let idle = BTreeMap::from([(v, Duration::from_secs(30))]);

        assert_eq!(
            pick_victim(&placement, &pressure, &idle, Pressure::new(0.80), &compat()),
            None
        );
    }

    /// Hottest host is over the watermark but every other host is just as hot —
    /// moving a VM cannot relieve pressure, so no move.
    #[test]
    fn no_move_when_no_host_is_cooler() {
        let v = vm();
        let mut placement = Placement::new();
        placement.assign(host("a"), v);
        let pressure = BTreeMap::from([
            (host("a"), Pressure::new(0.95)),
            (host("b"), Pressure::new(0.95)),
        ]);
        let idle = BTreeMap::from([(v, Duration::from_secs(30))]);

        assert_eq!(
            pick_victim(&placement, &pressure, &idle, Pressure::new(0.80), &compat()),
            None
        );
    }

    /// A hot host with no VMs cannot be relieved — no move.
    #[test]
    fn no_move_when_hottest_host_is_empty() {
        let placement = Placement::new(); // host-a has no VMs recorded
        let pressure = BTreeMap::from([
            (host("a"), Pressure::new(0.99)),
            (host("b"), Pressure::new(0.10)),
        ]);
        let idle = BTreeMap::new();

        assert_eq!(
            pick_victim(&placement, &pressure, &idle, Pressure::new(0.80), &compat()),
            None
        );
    }

    /// A VM with no idleness reading is treated as fully busy, so a VM that *is*
    /// known idle wins even with a small gap.
    #[test]
    fn missing_idleness_counts_as_busy() {
        let unknown = vm();
        let known_idle = vm();
        let mut placement = Placement::new();
        placement.assign(host("a"), unknown);
        placement.assign(host("a"), known_idle);
        let pressure = BTreeMap::from([
            (host("a"), Pressure::new(0.95)),
            (host("b"), Pressure::new(0.10)),
        ]);
        let idle = BTreeMap::from([(known_idle, Duration::from_millis(1))]);

        let pick = pick_victim(&placement, &pressure, &idle, Pressure::new(0.80), &compat())
            .expect("a move should be chosen");
        assert_eq!(pick.vm, known_idle);
    }

    /// The only cooler host is CPU-incompatible (different TSC) — restoring there
    /// would fail, so no move is chosen even though the source is hot.
    #[test]
    fn no_move_when_only_cooler_host_is_incompatible() {
        let v = vm();
        let mut placement = Placement::new();
        placement.assign(host("a"), v);
        let pressure = BTreeMap::from([
            (host("a"), Pressure::new(0.95)),
            (host("b"), Pressure::new(0.10)),
        ]);
        let idle = BTreeMap::from([(v, Duration::from_secs(30))]);
        // a at 1995 MHz, b at 2100 MHz — incompatible (the real incident).
        let compat = BTreeMap::from([(host("a"), klass(1_995_000)), (host("b"), klass(2_100_000))]);

        assert_eq!(
            pick_victim(&placement, &pressure, &idle, Pressure::new(0.80), &compat),
            None
        );
    }

    /// A cooler incompatible host is skipped in favour of a compatible host that
    /// is cooler than the source (even if warmer than the incompatible one).
    #[test]
    fn skips_incompatible_cooler_host_for_a_compatible_one() {
        let v = vm();
        let mut placement = Placement::new();
        placement.assign(host("a"), v);
        let pressure = BTreeMap::from([
            (host("a"), Pressure::new(0.95)), // hot source
            (host("b"), Pressure::new(0.05)), // coolest, but incompatible
            (host("c"), Pressure::new(0.40)), // compatible, still cooler than a
        ]);
        let idle = BTreeMap::from([(v, Duration::from_secs(30))]);
        let compat = BTreeMap::from([
            (host("a"), klass(2_000_000)),
            (host("b"), klass(2_100_000)), // incompatible TSC
            (host("c"), klass(2_000_000)), // compatible
        ]);

        let pick = pick_victim(&placement, &pressure, &idle, Pressure::new(0.80), &compat)
            .expect("a compatible move exists");
        assert_eq!(pick.to, host("c"));
    }

    /// Pressure construction clamps and tames non-finite input.
    #[test]
    fn pressure_clamps_and_rejects_non_finite() {
        assert_eq!(Pressure::new(1.5).get(), 1.0);
        assert_eq!(Pressure::new(-0.5).get(), 0.0);
        assert_eq!(Pressure::new(f64::NAN).get(), 0.0);
        assert_eq!(Pressure::new(0.42).get(), 0.42);
    }
}
