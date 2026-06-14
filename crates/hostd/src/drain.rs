//! Host-side drain coordination.
//!
//! [`DrainCoordinator`] is the host half of the drain protocol's decision: it
//! turns the raw signals arriving during a drain — the guest's `DrainAck` and
//! turn boundaries off the wire, plus locally-sampled infra and storage state —
//! into a single [`DrainVerdict`] for the migration driver.
//!
//! It is deliberately a **pure folder**, not an async loop: each `observe_*`
//! call updates the layered [`QuiescenceDetector`], and [`verdict`] reports
//! whether *all three* layers are quiet. The verdict is **verified, not
//! assumed** — it is [`Busy`](DrainVerdict::Busy) until positive evidence makes
//! every layer quiet, so a missing signal never reads as safe-to-migrate. The
//! async wiring that drives it (recv off the vsock, a sample ticker, the
//! deadline timer, sending `DrainRequest`/`DrainCancel`) owns the clock and
//! belongs to the real executor; keeping the decision pure here is what makes it
//! testable without a clock, a socket, or a VM.

use proto::{GuestToHost, TurnId};

use crate::quiesce::{InfraThresholds, QuiescenceDetector, QuiescenceReport};

/// The outcome of a drain attempt, as the migration driver consumes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainVerdict {
    /// All three quiescence layers are quiet — safe to snapshot.
    Quiescent,
    /// Not yet quiescent. By the race rule the migration must wait (until the
    /// deadline) or abort; the VM is never snapshotted in this state.
    Busy {
        /// The turn still in flight when the app layer is the hold-out, for the
        /// abort log line. `None` means the app layer is quiet but an infra or
        /// storage layer is not.
        in_flight: Option<TurnId>,
    },
}

/// Folds the signals of one drain attempt into a quiescence verdict.
#[derive(Debug)]
pub struct DrainCoordinator {
    detector: QuiescenceDetector,
}

impl DrainCoordinator {
    /// A coordinator for a fresh drain. All layers start active (not quiet); the
    /// infra layer uses `infra` thresholds.
    #[must_use]
    pub fn new(infra: InfraThresholds) -> Self {
        Self {
            detector: QuiescenceDetector::new(infra),
        }
    }

    /// Fold a message received from the guest into the app layer. Messages that
    /// do not bear on quiescence (handshake, resume, liveness) are ignored.
    pub fn observe_guest(&mut self, msg: &GuestToHost) {
        match msg {
            GuestToHost::TurnStarted { turn_id, .. } => self.detector.app.turn_started(*turn_id),
            GuestToHost::TurnEnded { .. } => self.detector.app.turn_ended(),
            GuestToHost::DrainAck { in_flight } => self.detector.app.drain_acked(*in_flight),
            GuestToHost::Hello { .. }
            | GuestToHost::Resumed { .. }
            | GuestToHost::Ping
            | GuestToHost::Pong => {}
        }
    }

    /// Record one infra sampling tick (vCPU utilization and whether the virtio
    /// queues were quiet at that tick).
    pub fn observe_infra(&mut self, cpu_pct: f64, queues_quiet: bool) {
        self.detector.infra.record(cpu_pct, queues_quiet);
    }

    /// Record whether the workspace sync has caught up to backing storage.
    pub fn observe_storage(&mut self, caught_up: bool) {
        self.detector.storage.set_caught_up(caught_up);
    }

    /// The per-layer quiescence report, for logs, the `/metrics` gauge, and the
    /// `sleepwalk quiesce` inspection command.
    #[must_use]
    pub fn report(&self) -> QuiescenceReport {
        self.detector.report()
    }

    /// The current verdict: [`Quiescent`](DrainVerdict::Quiescent) iff every
    /// layer is quiet, else [`Busy`](DrainVerdict::Busy) carrying the app
    /// layer's hold-out turn (if any).
    #[must_use]
    pub fn verdict(&self) -> DrainVerdict {
        if self.detector.is_quiescent() {
            DrainVerdict::Quiescent
        } else {
            DrainVerdict::Busy {
                in_flight: self.detector.app.in_flight(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::Timestamp;

    fn thresholds() -> InfraThresholds {
        InfraThresholds {
            cpu_pct: 5.0,
            samples: 3,
        }
    }

    fn turn(n: u64) -> TurnId {
        TurnId::from_u64(n)
    }

    fn ts(n: u64) -> Timestamp {
        Timestamp::from_nanos(n)
    }

    /// A fresh coordinator has seen nothing, so every layer is active: the
    /// verdict is Busy with no hold-out turn. Quiescence is never assumed.
    #[test]
    fn starts_busy_with_no_evidence() {
        let c = DrainCoordinator::new(thresholds());
        assert_eq!(c.verdict(), DrainVerdict::Busy { in_flight: None });
    }

    /// The realistic drain path: the guest acks with a turn still running, infra
    /// and storage go quiet, and only when that turn ends does the verdict flip
    /// to Quiescent. The app layer is the last hold-out, reported by id.
    #[test]
    fn flips_to_quiescent_only_when_all_three_agree() {
        let mut c = DrainCoordinator::new(thresholds());

        // Guest gates new turns but reports one in flight (the race rule winner).
        c.observe_guest(&GuestToHost::DrainAck {
            in_flight: Some(turn(7)),
        });
        // Infra and storage reach quiet while the turn is still running.
        for _ in 0..3 {
            c.observe_infra(1.0, true);
        }
        c.observe_storage(true);
        assert_eq!(
            c.verdict(),
            DrainVerdict::Busy {
                in_flight: Some(turn(7))
            },
            "the in-flight turn keeps the app layer active"
        );

        // The turn finishes — now all three layers are quiet.
        c.observe_guest(&GuestToHost::TurnEnded {
            turn_id: turn(7),
            ts: ts(1),
        });
        assert_eq!(c.verdict(), DrainVerdict::Quiescent);
        assert!(c.report().is_quiescent());
    }

    /// App quiet but infra still busy is Busy with no hold-out turn — the field
    /// distinguishes "a turn is running" from "the machine is not yet idle".
    #[test]
    fn infra_holdout_is_busy_without_a_turn() {
        let mut c = DrainCoordinator::new(thresholds());
        c.observe_guest(&GuestToHost::DrainAck { in_flight: None });
        c.observe_storage(true);
        // Only two quiet samples — the window is not full.
        c.observe_infra(1.0, true);
        c.observe_infra(1.0, true);
        assert_eq!(c.verdict(), DrainVerdict::Busy { in_flight: None });
    }

    /// A turn that starts before the drain ack is the reported hold-out.
    #[test]
    fn turn_started_then_acked_is_the_holdout() {
        let mut c = DrainCoordinator::new(thresholds());
        c.observe_guest(&GuestToHost::TurnStarted {
            turn_id: turn(3),
            ts: ts(1),
        });
        c.observe_guest(&GuestToHost::DrainAck {
            in_flight: Some(turn(3)),
        });
        for _ in 0..3 {
            c.observe_infra(1.0, true);
        }
        c.observe_storage(true);
        assert_eq!(
            c.verdict(),
            DrainVerdict::Busy {
                in_flight: Some(turn(3))
            }
        );
    }

    /// Messages unrelated to quiescence do not move the verdict.
    #[test]
    fn liveness_and_handshake_are_ignored() {
        let mut c = DrainCoordinator::new(thresholds());
        c.observe_guest(&GuestToHost::DrainAck { in_flight: None });
        for _ in 0..3 {
            c.observe_infra(1.0, true);
        }
        c.observe_storage(true);
        assert_eq!(c.verdict(), DrainVerdict::Quiescent);

        // Pings and a resume notice leave the verdict untouched.
        c.observe_guest(&GuestToHost::Ping);
        c.observe_guest(&GuestToHost::Resumed { ts: ts(2) });
        assert_eq!(c.verdict(), DrainVerdict::Quiescent);
    }
}
