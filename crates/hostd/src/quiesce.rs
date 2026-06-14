//! The layered quiescence detector (objective O3).
//!
//! A VM is **quiescent** — safe to snapshot and move — only when *all three*
//! layers are simultaneously quiet:
//!
//! 1. [`AppLayer`] — the workload itself: new turns are gated and none is in
//!    flight (ground truth from guestd's `DrainAck` / turn signals).
//! 2. [`InfraLayer`] — the machine: vCPU utilization has stayed below a
//!    threshold for N consecutive samples and the virtio queues are quiet.
//!    Catches background work the app never reported (a stray `npm install`).
//! 3. [`StorageLayer`] — durable state: the workspace sync has caught up to
//!    backing storage.
//!
//! The whole point is that quiescence is *verified, not assumed*: every layer
//! defaults to **active** (not quiet) until it has positive evidence otherwise,
//! so a missing signal never reads as "safe to migrate". The data sources (real
//! `/proc` sampling, the storage watermark) are fed in from the edges; the logic
//! here is pure.

use std::collections::VecDeque;

use proto::TurnId;

/// The app layer: gated + nothing in flight.
///
/// hostd updates this from the guest's vsock stream — drain acks and turn
/// boundaries. It is quiet only once a drain has gated new turns *and* the
/// in-flight turn (if any) has ended. This is the app-layer half of the race
/// rule: while a turn runs, the layer is active and no migration can proceed.
#[derive(Debug, Clone, Default)]
pub struct AppLayer {
    gated: bool,
    in_flight: Option<TurnId>,
}

impl AppLayer {
    /// A fresh layer: not gated, so not quiet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A turn started — the workload is busy.
    pub fn turn_started(&mut self, turn: TurnId) {
        self.in_flight = Some(turn);
    }

    /// A turn ended — clear the in-flight marker.
    pub fn turn_ended(&mut self) {
        self.in_flight = None;
    }

    /// The guest acked a drain, reporting what (if anything) is still in flight.
    /// This closes the gate.
    pub fn drain_acked(&mut self, in_flight: Option<TurnId>) {
        self.gated = true;
        self.in_flight = in_flight;
    }

    /// The drain was cancelled — reopen the gate.
    pub fn drain_cancelled(&mut self) {
        self.gated = false;
    }

    /// The turn currently reported in flight, if any. The hold-out turn when the
    /// app layer is what keeps a drain from reaching quiescence.
    #[must_use]
    pub fn in_flight(&self) -> Option<TurnId> {
        self.in_flight
    }

    /// Quiet iff new turns are gated and none is in flight.
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        self.gated && self.in_flight.is_none()
    }
}

/// Thresholds for the infra layer (config keys; tuned in measurement).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InfraThresholds {
    /// A sample is "quiet" if vCPU utilization is below this percentage.
    pub cpu_pct: f64,
    /// How many consecutive quiet samples are required.
    pub samples: usize,
}

/// The infra layer: vCPU quiet for N consecutive samples and queues drained.
///
/// Holds a sliding window of the most recent samples. It is quiet only once the
/// window is full *and* every sample in it is below `cpu_pct` *and* the virtio
/// queues are reported quiet — so a single CPU spike resets the evidence.
#[derive(Debug, Clone)]
pub struct InfraLayer {
    thresholds: InfraThresholds,
    recent: VecDeque<f64>,
    queues_quiet: bool,
}

impl InfraLayer {
    /// A fresh layer with no samples yet (so: active).
    #[must_use]
    pub fn new(thresholds: InfraThresholds) -> Self {
        Self {
            thresholds,
            recent: VecDeque::with_capacity(thresholds.samples.max(1)),
            queues_quiet: false,
        }
    }

    /// Record one sampling tick: the vCPU utilization and whether the virtio
    /// queues were quiet at that tick.
    pub fn record(&mut self, cpu_pct: f64, queues_quiet: bool) {
        if self.recent.len() == self.thresholds.samples {
            self.recent.pop_front();
        }
        self.recent.push_back(cpu_pct);
        self.queues_quiet = queues_quiet;
    }

    /// Quiet iff the window is full, every sample is below `cpu_pct`, and the
    /// queues are quiet. An empty/partial window is never quiet.
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        self.thresholds.samples > 0
            && self.recent.len() == self.thresholds.samples
            && self.queues_quiet
            && self.recent.iter().all(|&c| c < self.thresholds.cpu_pct)
    }
}

/// The storage layer: workspace sync caught up to backing storage.
#[derive(Debug, Clone, Default)]
pub struct StorageLayer {
    caught_up: bool,
}

impl StorageLayer {
    /// A fresh layer: not caught up, so not quiet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Report whether the sync watermark has caught up to backing storage.
    pub fn set_caught_up(&mut self, caught_up: bool) {
        self.caught_up = caught_up;
    }

    /// Quiet iff the sync has caught up.
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        self.caught_up
    }
}

/// A per-layer snapshot of quiescence, for logs, the `/metrics` gauge, and the
/// `sleepwalk quiesce` inspection command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuiescenceReport {
    /// Whether the app layer is quiet.
    pub app: bool,
    /// Whether the infra layer is quiet.
    pub infra: bool,
    /// Whether the storage layer is quiet.
    pub storage: bool,
}

impl QuiescenceReport {
    /// Quiescent iff every layer is quiet.
    #[must_use]
    pub fn is_quiescent(&self) -> bool {
        self.app && self.infra && self.storage
    }
}

/// Combines the three layers into one verdict.
#[derive(Debug, Clone)]
pub struct QuiescenceDetector {
    /// The app-layer state.
    pub app: AppLayer,
    /// The infra-layer state.
    pub infra: InfraLayer,
    /// The storage-layer state.
    pub storage: StorageLayer,
}

impl QuiescenceDetector {
    /// Build a detector with the given infra thresholds; app and storage start
    /// active.
    #[must_use]
    pub fn new(infra: InfraThresholds) -> Self {
        Self {
            app: AppLayer::new(),
            infra: InfraLayer::new(infra),
            storage: StorageLayer::new(),
        }
    }

    /// The per-layer report.
    #[must_use]
    pub fn report(&self) -> QuiescenceReport {
        QuiescenceReport {
            app: self.app.is_quiet(),
            infra: self.infra.is_quiet(),
            storage: self.storage.is_quiet(),
        }
    }

    /// Whether all three layers are quiet right now.
    #[must_use]
    pub fn is_quiescent(&self) -> bool {
        self.report().is_quiescent()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thresholds() -> InfraThresholds {
        InfraThresholds {
            cpu_pct: 5.0,
            samples: 3,
        }
    }

    #[test]
    fn app_layer_needs_gated_and_idle() {
        let mut app = AppLayer::new();
        assert!(!app.is_quiet(), "ungated is active");

        app.turn_started(TurnId::FIRST);
        app.drain_acked(Some(TurnId::FIRST)); // gated but a turn is in flight
        assert!(!app.is_quiet(), "in-flight turn keeps it active");

        app.turn_ended();
        assert!(app.is_quiet(), "gated + idle is quiet");

        app.drain_cancelled();
        assert!(!app.is_quiet(), "un-gating reactivates");
    }

    #[test]
    fn infra_layer_needs_a_full_quiet_window() {
        let mut infra = InfraLayer::new(thresholds());
        assert!(!infra.is_quiet(), "no samples yet");

        infra.record(1.0, true);
        infra.record(2.0, true);
        assert!(!infra.is_quiet(), "window not full");
        infra.record(3.0, true);
        assert!(infra.is_quiet(), "three quiet samples + quiet queues");
    }

    #[test]
    fn infra_layer_spike_resets_the_evidence() {
        let mut infra = InfraLayer::new(thresholds());
        for _ in 0..3 {
            infra.record(1.0, true);
        }
        assert!(infra.is_quiet());

        infra.record(80.0, true); // a spike enters the window
        assert!(!infra.is_quiet(), "spike in window");

        // The spike only clears once it has aged fully out of the window — it
        // takes `samples` (3) fresh quiet readings, not fewer.
        infra.record(1.0, true);
        infra.record(1.0, true);
        assert!(!infra.is_quiet(), "spike still in the 3-wide window");
        infra.record(1.0, true);
        assert!(infra.is_quiet(), "spike aged out, three quiet again");
    }

    #[test]
    fn infra_layer_busy_queues_block_quiescence() {
        let mut infra = InfraLayer::new(thresholds());
        infra.record(1.0, true);
        infra.record(1.0, true);
        infra.record(1.0, false); // queues active on the latest tick
        assert!(!infra.is_quiet());
    }

    #[test]
    fn detector_is_quiescent_only_when_all_three_agree() {
        let mut d = QuiescenceDetector::new(thresholds());
        // Drive each layer to quiet one at a time; only the last flip is enough.
        assert!(!d.is_quiescent());

        d.app.drain_acked(None);
        assert_eq!(
            d.report(),
            QuiescenceReport {
                app: true,
                infra: false,
                storage: false
            }
        );
        assert!(!d.is_quiescent());

        for _ in 0..3 {
            d.infra.record(1.0, true);
        }
        assert!(!d.is_quiescent(), "storage still active");

        d.storage.set_caught_up(true);
        assert!(d.is_quiescent(), "all three quiet");
        assert!(d.report().is_quiescent());

        // Any single layer going active drops quiescence again.
        d.app.drain_cancelled();
        assert!(!d.is_quiescent());
    }
}
