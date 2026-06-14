//! The turn-vs-drain chaos harness (objective O4).
//!
//! The race rule (`docs/protocol.md`) is a safety claim: a migration may never
//! drop a turn or cut one short. This module *falsifies* that claim cheaply —
//! at the mock layer, with a deterministic fake clock, thousands of
//! interleavings per second — before the same property is checked against real
//! VMs on the wall clock (the integration tier, `/dev/kvm`).
//!
//! One [`simulate`] run drops a single `DrainRequest` at a random offset into a
//! stream of sequential turns, drives a real [`Guest`] through the resulting
//! event interleaving, then resumes and replays the backlog — exactly the path
//! a migration takes. The whole timeline is ordered by an integer fake clock, so
//! a `seed` fully determines the run and a failure reproduces from the seed
//! alone. [`RaceReport::race_rule_holds`] encodes the three invariants the rule
//! demands.

use proto::{GuestToHost, GuestdVersion, HostToGuest, Timestamp, TurnId, VmId};
use std::time::Duration;

use guestd::{Guest, GuestError, PseudoChannel, StartOutcome};

use crate::schedule::SplitMix64;

/// One tick of the fake clock, in nanoseconds, so distinct events get distinct,
/// ordered timestamps without colliding.
const TICK_NS: u64 = 1_000_000;

/// The observable facts of one chaos run — everything the race rule constrains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RaceReport {
    /// Turns the workload tried to start over the run.
    pub attempted: u32,
    /// Turns that actually ran to completion (started *and* ended), including
    /// the in-flight winner and every replayed backlog turn.
    pub completed: u32,
    /// What the guest's `DrainAck` reported as in flight at the drain instant.
    pub ack_in_flight: Option<TurnId>,
    /// What was actually in flight at the drain instant, derived independently
    /// from the guest's emitted `TurnStarted`/`TurnEnded` transcript.
    pub wire_in_flight: Option<TurnId>,
    /// How many turns started *after* the gate closed. Must be zero: a gated
    /// turn is queued, never run.
    pub started_while_gated: u32,
}

impl RaceReport {
    /// Whether this run upheld the race rule on all three counts:
    /// 1. zero dropped turns (every attempt eventually completed),
    /// 2. the `DrainAck` named exactly the turn in flight at the drain instant,
    /// 3. no turn started while the gate was closed.
    #[must_use]
    pub fn race_rule_holds(&self) -> bool {
        self.completed == self.attempted
            && self.ack_in_flight == self.wire_in_flight
            && self.started_while_gated == 0
    }

    /// Whether a turn was in flight when the drain landed (the drain hit a busy
    /// window rather than an idle gap). Used to confirm the seed corpus
    /// exercises both outcomes.
    #[must_use]
    pub fn drain_hit_busy(&self) -> bool {
        self.ack_in_flight.is_some()
    }
}

/// A turn's place on the fake-clock timeline.
#[derive(Debug, Clone, Copy)]
struct Turn {
    start: u64,
    end: u64,
}

/// Event kinds, ordered so ties on the same tick resolve per the race rule:
/// a turn that ends frees the slot before the next one starts, and a turn that
/// starts on the same tick as the drain counts as in flight (it is emitted
/// before the drain is processed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    End = 0,
    Start = 1,
    Drain = 2,
}

/// Run one deterministic chaos iteration for `seed`.
///
/// Generates 3–8 sequential, non-overlapping turns, drops a drain at a random
/// offset within their span, drives a [`Guest`] through the time-ordered events,
/// then resumes and replays the backlog. The returned [`RaceReport`] carries the
/// facts to assert against; the run touches no real clock, socket, or VM.
///
/// # Errors
/// Propagates any [`GuestError`] from the supervisor — none should occur for a
/// well-formed interleaving, so an error is itself a falsification.
pub async fn simulate(seed: u64) -> Result<RaceReport, GuestError> {
    let mut rng = SplitMix64::new(seed);
    let (turns, drain_at) = generate(&mut rng);

    // Build the time-ordered event list: (tick, kind, turn index).
    let mut events: Vec<(u64, Kind, usize)> = Vec::with_capacity(turns.len() * 2 + 1);
    for (i, t) in turns.iter().enumerate() {
        events.push((t.start, Kind::Start, i));
        events.push((t.end, Kind::End, i));
    }
    events.push((drain_at, Kind::Drain, usize::MAX));
    events.sort_by_key(|&(tick, kind, i)| (tick, kind, i));

    // The VM identity is irrelevant to the race — only its turn stream matters.
    let mut g = Guest::new(
        VmId::new(),
        GuestdVersion::new("0.1.0"),
        PseudoChannel::new(),
    );

    // Each turn index, once started, remembers its assigned id so its End event
    // can be matched to the turn actually in flight.
    let mut started: Vec<Option<TurnId>> = vec![None; turns.len()];
    let mut drained = false;
    let mut completed: u32 = 0;
    let mut started_while_gated: u32 = 0;

    for (tick, kind, i) in events {
        let now = Timestamp::from_nanos(tick.saturating_mul(TICK_NS));
        match kind {
            Kind::Start => match g.start_turn(now).await? {
                StartOutcome::Started(id) => {
                    if drained {
                        started_while_gated += 1;
                    }
                    started[i] = Some(id);
                }
                StartOutcome::Queued => {}
            },
            Kind::End => {
                if let Some(id) = started[i]
                    && g.in_flight() == Some(id)
                {
                    g.end_turn(now).await?;
                    completed += 1;
                }
            }
            Kind::Drain => {
                g.handle(HostToGuest::DrainRequest {
                    deadline: Duration::from_millis(500),
                })
                .await?;
                drained = true;
            }
        }
    }

    // Migrate: resume on the target host and replay the queued backlog. A fresh
    // clock region (past every event tick) keeps timestamps monotonic.
    let mut t = drain_at.max(turns.last().map_or(0, |x| x.end)) + 1;
    g.resume(Timestamp::from_nanos(t.saturating_mul(TICK_NS)))
        .await?;
    loop {
        t += 1;
        let now = Timestamp::from_nanos(t.saturating_mul(TICK_NS));
        if g.replay_next(now).await?.is_none() {
            break;
        }
        t += 1;
        let end = Timestamp::from_nanos(t.saturating_mul(TICK_NS));
        g.end_turn(end).await?;
        completed += 1;
    }

    let (ack_in_flight, wire_in_flight) = read_ack(&g.channel().sent());

    Ok(RaceReport {
        attempted: turns.len() as u32,
        completed,
        ack_in_flight,
        wire_in_flight,
        started_while_gated,
    })
}

/// Generate sequential, non-overlapping turns and a drain offset within their
/// span. Turns are spaced by random idle gaps, so a drain lands sometimes inside
/// a turn (busy) and sometimes in a gap (idle) as the seed varies.
fn generate(rng: &mut SplitMix64) -> (Vec<Turn>, u64) {
    let k = 3 + (rng.next_u64() % 6) as usize; // 3..=8 turns
    let mut turns = Vec::with_capacity(k);
    let mut cursor = 0u64;
    for _ in 0..k {
        let gap = rng.next_u64() % 120; // idle before this turn
        let dur = 20 + rng.next_u64() % 150; // turn length 20..=169
        let start = cursor + gap;
        let end = start + dur;
        turns.push(Turn { start, end });
        cursor = end;
    }
    let span = cursor.max(1);
    let drain_at = rng.next_u64() % span;
    (turns, drain_at)
}

/// From the guest's outbound transcript, return the `DrainAck`'s reported
/// `in_flight` alongside what was independently in flight (last `TurnStarted`
/// without a following `TurnEnded`) at the moment the ack was emitted.
fn read_ack(sent: &[GuestToHost]) -> (Option<TurnId>, Option<TurnId>) {
    let mut in_flight = None;
    for msg in sent {
        match msg {
            GuestToHost::TurnStarted { turn_id, .. } => in_flight = Some(*turn_id),
            GuestToHost::TurnEnded { .. } => in_flight = None,
            GuestToHost::DrainAck { in_flight: acked } => return (*acked, in_flight),
            _ => {}
        }
    }
    (None, in_flight)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The race rule holds across a large corpus of seeded interleavings, and
    /// the corpus exercises *both* outcomes: drains that land on a busy turn and
    /// drains that land in an idle gap. A corpus that only ever hit one would
    /// pass vacuously.
    #[tokio::test]
    async fn race_rule_holds_across_many_interleavings() {
        let mut busy = 0u32;
        let mut idle = 0u32;
        for seed in 0..2_000u64 {
            let r = simulate(seed).await.expect("simulation runs cleanly");
            assert!(
                r.race_rule_holds(),
                "race rule violated at seed {seed}: {r:?}"
            );
            if r.drain_hit_busy() {
                busy += 1;
            } else {
                idle += 1;
            }
        }
        assert!(busy > 0, "corpus never drained a busy turn");
        assert!(idle > 0, "corpus never drained during an idle gap");
    }

    /// A run reproduces exactly from its seed.
    #[tokio::test]
    async fn simulation_is_deterministic_for_a_seed() {
        let a = simulate(123).await.expect("run a");
        let b = simulate(123).await.expect("run b");
        assert_eq!(a, b);
    }
}
