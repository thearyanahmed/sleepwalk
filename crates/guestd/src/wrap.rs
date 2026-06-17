//! Wrap mode: supervise an arbitrary child process and infer turn boundaries
//! from its stdout, so a workload that knows nothing about the vsock protocol
//! still produces the busy/idle signal hostd's quiescence detector reads.
//!
//! The contract is one marker line per boundary: a configurable start marker
//! opens a turn, an end marker closes it. Every other line the child prints is
//! ignored by the classifier (the binary forwards it for logging). This is the
//! zero-code adoption path (see "Adoption modes" in `docs/protocol.md`): the
//! workload declares its turn signal, guestd translates it to
//! `TurnStarted`/`TurnEnded` over vsock.
//!
//! Wrap mode only *observes*. It cannot defer a turn the child has already
//! begun, so drain is **passive**: [`begin_observed_turn`](Guest::begin_observed_turn)
//! keeps `in_flight` truthful, the `DrainAck` reports it, and hostd waits until
//! the child is between turns before snapshotting. Active gating (queue new
//! turns, replay after resume) is the native-mode path, where the workload
//! speaks the protocol itself.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use proto::{Timestamp, TurnId};

use crate::channel::GuestChannel;
use crate::guest::{Guest, GuestError};

/// A turn boundary inferred from a line of child stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnSignal {
    /// The child opened a turn (it is now busy).
    Start,
    /// The child closed a turn (it is now idle).
    End,
}

/// How to recognise turn boundaries in a wrapped child's stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrapConfig {
    /// A line equal to this (after trimming) opens a turn.
    pub start_marker: String,
    /// A line equal to this (after trimming) closes a turn.
    pub end_marker: String,
}

impl Default for WrapConfig {
    fn default() -> Self {
        // Distinctive sentinels unlikely to collide with normal program output.
        Self {
            start_marker: "@@TURN_START@@".to_owned(),
            end_marker: "@@TURN_END@@".to_owned(),
        }
    }
}

impl WrapConfig {
    /// Classify one line of child stdout, or `None` if it is not a boundary.
    #[must_use]
    pub fn classify(&self, line: &str) -> Option<TurnSignal> {
        let line = line.trim();
        if line == self.start_marker {
            Some(TurnSignal::Start)
        } else if line == self.end_marker {
            Some(TurnSignal::End)
        } else {
            None
        }
    }
}

/// Apply an inferred boundary to the supervisor.
///
/// Lenient by design: the child is untrusted output, not a protocol peer, so a
/// redundant `Start` while a turn is already in flight and a stray `End` with no
/// turn are both ignored rather than erroring. A clean alternation produces one
/// `TurnStarted` then one `TurnEnded` on the wire.
pub async fn apply_signal<C: GuestChannel>(
    guest: &mut Guest<C>,
    signal: TurnSignal,
    now: Timestamp,
) -> Result<(), GuestError> {
    match signal {
        TurnSignal::Start => {
            if guest.in_flight().is_some() {
                return Ok(()); // already mid-turn; ignore the duplicate open
            }
            guest.begin_observed_turn(now).await.map(|_| ())
        }
        TurnSignal::End => {
            if guest.in_flight().is_none() {
                return Ok(()); // no turn open; ignore the stray close
            }
            guest.end_turn(now).await
        }
    }
}

/// Turn state shared between the continuous child-stdout reader (which updates it
/// from inferred boundaries) and the drain responders on each host connection —
/// both the vsock loop and the TCP drain channel. This is what lets a *passive*
/// wrap-mode guest answer "am I mid-turn right now?" correctly even when no host
/// is currently connected: the reader runs continuously, so when a drain arrives
/// (over either transport) the `DrainAck` reflects the live turn state and a
/// migration is never allowed to snapshot mid-turn.
///
/// Cheap to clone (an `Arc`); clones share one state.
#[derive(Clone, Default)]
pub struct TurnTracker(Arc<TrackerInner>);

#[derive(Default)]
struct TrackerInner {
    in_turn: AtomicBool,
    last_id: AtomicU64,
}

impl TurnTracker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Update from an inferred boundary. Lenient like [`apply_signal`]: a
    /// duplicate open or a stray close just sets the flag and is otherwise
    /// ignored (the child is untrusted output, not a protocol peer). The turn id
    /// advances only on a genuine open.
    pub fn apply(&self, signal: TurnSignal) {
        match signal {
            TurnSignal::Start => {
                if !self.0.in_turn.swap(true, Ordering::Relaxed) {
                    self.0.last_id.fetch_add(1, Ordering::Relaxed);
                }
            }
            TurnSignal::End => self.0.in_turn.store(false, Ordering::Relaxed),
        }
    }

    /// The in-flight turn id, or `None` between turns — the `DrainAck` payload.
    #[must_use]
    pub fn in_flight(&self) -> Option<TurnId> {
        if self.0.in_turn.load(Ordering::Relaxed) {
            Some(TurnId::from_u64(self.0.last_id.load(Ordering::Relaxed)))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use proto::{GuestToHost, GuestdVersion, VmId};

    use super::*;
    use crate::pseudo_channel::PseudoChannel;

    fn guest() -> Guest<PseudoChannel> {
        Guest::new(
            VmId::from_uuid(uuid::Uuid::nil()),
            GuestdVersion::new("0.1.0"),
            PseudoChannel::new(),
        )
    }

    fn ts(n: u64) -> Timestamp {
        Timestamp::from_nanos(n)
    }

    #[test]
    fn classify_matches_markers_and_ignores_other_lines() {
        let cfg = WrapConfig::default();
        assert_eq!(cfg.classify("@@TURN_START@@"), Some(TurnSignal::Start));
        assert_eq!(cfg.classify("@@TURN_END@@"), Some(TurnSignal::End));
        // Trimming: trailing newline / surrounding whitespace still match.
        assert_eq!(cfg.classify("  @@TURN_START@@\n"), Some(TurnSignal::Start));
        // Ordinary output is not a boundary.
        assert_eq!(cfg.classify("computed sequence value 7"), None);
        // A marker embedded in a larger line does not match (exact line only).
        assert_eq!(cfg.classify("prefix @@TURN_START@@"), None);
    }

    #[test]
    fn classify_honours_custom_markers() {
        let cfg = WrapConfig {
            start_marker: ">>begin".to_owned(),
            end_marker: ">>end".to_owned(),
        };
        assert_eq!(cfg.classify(">>begin"), Some(TurnSignal::Start));
        assert_eq!(cfg.classify("@@TURN_START@@"), None);
    }

    #[tokio::test]
    async fn clean_alternation_emits_one_started_then_ended() {
        let mut g = guest();
        apply_signal(&mut g, TurnSignal::Start, ts(1))
            .await
            .expect("start");
        assert!(g.in_flight().is_some());
        apply_signal(&mut g, TurnSignal::End, ts(2))
            .await
            .expect("end");
        assert!(g.in_flight().is_none());
        assert!(matches!(
            g.channel().sent().as_slice(),
            [
                GuestToHost::TurnStarted { .. },
                GuestToHost::TurnEnded { .. },
            ]
        ));
    }

    #[tokio::test]
    async fn duplicate_start_is_ignored() {
        let mut g = guest();
        apply_signal(&mut g, TurnSignal::Start, ts(1))
            .await
            .expect("start");
        // A second start with a turn already in flight must not open another.
        apply_signal(&mut g, TurnSignal::Start, ts(2))
            .await
            .expect("dup");
        assert_eq!(g.channel().sent().len(), 1); // still just the one TurnStarted
    }

    #[tokio::test]
    async fn stray_end_is_ignored() {
        let mut g = guest();
        apply_signal(&mut g, TurnSignal::End, ts(1))
            .await
            .expect("stray");
        assert!(g.channel().sent().is_empty());
    }

    #[tokio::test]
    async fn observed_turn_bypasses_the_drain_gate() {
        use proto::HostToGuest;
        use std::time::Duration;

        let mut g = guest();

        // Gate closes (a migration drain started)…
        g.handle(
            HostToGuest::DrainRequest {
                deadline: Duration::from_secs(5),
            },
            ts(0),
        )
        .await
        .expect("drain");
        assert!(g.is_gated());

        // …but the wrapped child started a turn anyway. Wrap mode cannot defer
        // external work, so it must report it (keeping DrainAck truthful), not
        // queue it.
        apply_signal(&mut g, TurnSignal::Start, ts(1))
            .await
            .expect("observed start");
        assert!(g.in_flight().is_some());
        assert_eq!(g.queued_turns(), 0);
        assert!(matches!(
            g.channel().sent().last(),
            Some(GuestToHost::TurnStarted { .. })
        ));
    }

    #[test]
    fn tracker_starts_quiescent() {
        assert_eq!(TurnTracker::new().in_flight(), None);
    }

    #[test]
    fn tracker_tracks_open_and_close() {
        let t = TurnTracker::new();
        t.apply(TurnSignal::Start);
        assert!(t.in_flight().is_some(), "in a turn after start");
        t.apply(TurnSignal::End);
        assert_eq!(t.in_flight(), None, "between turns after end");
    }

    #[test]
    fn tracker_id_advances_only_on_genuine_open() {
        let t = TurnTracker::new();
        t.apply(TurnSignal::Start);
        let first = t.in_flight();
        t.apply(TurnSignal::Start); // duplicate open — ignored
        assert_eq!(t.in_flight(), first, "duplicate start does not bump the id");
        t.apply(TurnSignal::End);
        t.apply(TurnSignal::Start); // next genuine turn
        assert_ne!(t.in_flight(), first, "a new turn gets a new id");
    }

    #[test]
    fn tracker_ignores_stray_close() {
        let t = TurnTracker::new();
        t.apply(TurnSignal::End);
        assert_eq!(t.in_flight(), None);
    }

    #[test]
    fn tracker_clone_shares_state() {
        let a = TurnTracker::new();
        let b = a.clone();
        a.apply(TurnSignal::Start);
        assert!(b.in_flight().is_some(), "clone observes the same state");
    }
}
