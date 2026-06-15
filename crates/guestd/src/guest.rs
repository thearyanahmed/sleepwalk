//! The supervisor state machine.
//!
//! [`Guest`] is the in-VM half of the protocol. It owns a [`GuestChannel`] and
//! the turn/drain state, and exposes the operations the synthetic turn loop (and
//! later, a real wrapped process) drive:
//!
//! - [`handshake`](Guest::handshake): announce identity, take secrets at boot.
//! - [`start_turn`](Guest::start_turn) / [`end_turn`](Guest::end_turn): emit the
//!   ground-truth busy/idle signals hostd's quiescence detector reads.
//! - [`handle`](Guest::handle): react to a host message — most importantly
//!   `DrainRequest`, which closes the gate on new turns.
//!
//! The drain gate is the guest's half of the race rule (`docs/protocol.md`): a
//! turn already in flight when a drain arrives is reported in the `DrainAck` and
//! is never cut short; turns that arrive *after* the gate closes are not run but
//! are *queued in-guest* ([`StartOutcome::Queued`]) and replayed once the gate
//! reopens — either after a [`resume`](Guest::resume) on the target host, or
//! after a [`DrainCancel`](proto::HostToGuest::DrainCancel) aborts the
//! migration. The backlog is plain in-RAM state, so it survives the snapshot and
//! travels with the VM. This is what "zero dropped turns" in the race rule
//! means: a gated turn is deferred, never lost.

use std::collections::BTreeMap;

use proto::{GuestToHost, GuestdVersion, HostToGuest, Timestamp, TurnId, VmId};
use thiserror::Error;

use crate::channel::{ChannelError, GuestChannel};

/// What happened when the workload tried to start a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartOutcome {
    /// The turn started; here is its id (`TurnStarted` was sent).
    Started(TurnId),
    /// The drain gate is closed — the turn was not run but was queued in-guest.
    /// It will replay (via [`replay_next`](Guest::replay_next)) once the gate
    /// reopens on resume or drain-cancel. Nothing was sent on the wire.
    Queued,
}

/// Why a supervisor operation failed.
#[derive(Debug, Error)]
pub enum GuestError {
    /// The channel to hostd failed.
    #[error(transparent)]
    Channel(#[from] ChannelError),

    /// `handshake` expected `Secrets` as the reply to `Hello` but got something
    /// else.
    #[error("expected Secrets after Hello, got a different message")]
    UnexpectedHandshake,

    /// A turn was started while one was already in flight. The synthetic loop is
    /// sequential; overlapping turns are a caller bug.
    #[error("cannot start a turn while turn {in_flight:?} is in flight")]
    TurnInProgress {
        /// The turn already running.
        in_flight: TurnId,
    },

    /// `end_turn` was called with no turn in flight.
    #[error("no turn in flight to end")]
    NoTurnInFlight,

    /// `replay_next` was called while the drain gate is still closed. Queued
    /// turns may only replay once the gate has reopened (resume or cancel).
    #[error("cannot replay a queued turn while the drain gate is closed")]
    GatedReplay,
}

/// The in-VM supervisor for one VM.
#[derive(Debug)]
pub struct Guest<C: GuestChannel> {
    vm_id: VmId,
    version: GuestdVersion,
    chan: C,
    secrets: BTreeMap<String, String>,
    next_turn: TurnId,
    in_flight: Option<TurnId>,
    /// True once a `DrainRequest` has closed the gate; new turns are queued.
    gated: bool,
    /// Backlog of turns refused while gated, awaiting replay once the gate
    /// reopens. Synthetic turns are interchangeable work units, so a count is
    /// enough to prove none were dropped; this becomes a queue of work items
    /// once turns carry real per-turn tasks (the real-agent profile).
    queued: u32,
}

impl<C: GuestChannel> Guest<C> {
    /// Create a supervisor that will identify as `vm_id` / `version`.
    pub fn new(vm_id: VmId, version: GuestdVersion, chan: C) -> Self {
        Self {
            vm_id,
            version,
            chan,
            secrets: BTreeMap::new(),
            next_turn: TurnId::FIRST,
            in_flight: None,
            gated: false,
            queued: 0,
        }
    }

    /// The turn in flight, if any.
    #[must_use]
    pub fn in_flight(&self) -> Option<TurnId> {
        self.in_flight
    }

    /// Whether the drain gate is closed (new turns queued, not run).
    #[must_use]
    pub fn is_gated(&self) -> bool {
        self.gated
    }

    /// How many turns are queued for replay (refused while gated).
    #[must_use]
    pub fn queued_turns(&self) -> u32 {
        self.queued
    }

    /// The secrets received at boot (the env handed to the workload).
    #[must_use]
    pub fn secrets(&self) -> &BTreeMap<String, String> {
        &self.secrets
    }

    /// Borrow the underlying channel — used by tests and the chaos harness to
    /// inspect what the supervisor emitted on the wire.
    #[must_use]
    pub fn channel(&self) -> &C {
        &self.chan
    }

    /// Boot handshake: send `Hello`, then take the `Secrets` reply.
    ///
    /// The secrets are kept in memory only (never written to the rootfs or the
    /// kernel cmdline) and become the workload's environment.
    pub async fn handshake(&mut self) -> Result<(), GuestError> {
        self.chan
            .send(GuestToHost::Hello {
                vm_id: self.vm_id,
                guestd_version: self.version.clone(),
            })
            .await?;
        match self.chan.recv().await? {
            HostToGuest::Secrets { env } => {
                self.secrets = env;
                Ok(())
            }
            _ => Err(GuestError::UnexpectedHandshake),
        }
    }

    /// Start a turn. Queued with [`StartOutcome::Queued`] if the drain gate is
    /// closed; otherwise assigns the next id, marks it in flight, and sends
    /// `TurnStarted`.
    pub async fn start_turn(&mut self, now: Timestamp) -> Result<StartOutcome, GuestError> {
        if let Some(in_flight) = self.in_flight {
            return Err(GuestError::TurnInProgress { in_flight });
        }
        if self.gated {
            self.queued += 1;
            return Ok(StartOutcome::Queued);
        }
        Ok(StartOutcome::Started(self.begin_turn(now).await?))
    }

    /// Run one host-commanded turn (a [`RunTurn`](proto::HostToGuest::RunTurn))
    /// end to end under the host's `turn_id`: emit the busy then idle signal so
    /// the host can time and correlate it. Deferred with [`StartOutcome::Queued`]
    /// if the drain gate is closed — exactly like a self-driven turn, so a
    /// migration started mid-load still honours the race rule. The unit of work
    /// is the round trip itself; a wrapped real workload would do its work
    /// between the two signals.
    pub async fn run_turn(
        &mut self,
        turn_id: TurnId,
        now: Timestamp,
    ) -> Result<StartOutcome, GuestError> {
        if let Some(in_flight) = self.in_flight {
            return Err(GuestError::TurnInProgress { in_flight });
        }
        if self.gated {
            self.queued += 1;
            return Ok(StartOutcome::Queued);
        }
        self.in_flight = Some(turn_id);
        self.chan
            .send(GuestToHost::TurnStarted { turn_id, ts: now })
            .await?;
        self.chan
            .send(GuestToHost::TurnEnded { turn_id, ts: now })
            .await?;
        self.in_flight = None;
        Ok(StartOutcome::Started(turn_id))
    }

    /// Replay one turn from the backlog that built up while the gate was closed.
    ///
    /// Returns the started turn's id, or `None` when the backlog is empty. The
    /// gate must be open (post-resume or post-cancel) and no turn may be in
    /// flight — replaying is sequential, exactly like normal turn execution.
    pub async fn replay_next(&mut self, now: Timestamp) -> Result<Option<TurnId>, GuestError> {
        if let Some(in_flight) = self.in_flight {
            return Err(GuestError::TurnInProgress { in_flight });
        }
        if self.gated {
            return Err(GuestError::GatedReplay);
        }
        if self.queued == 0 {
            return Ok(None);
        }
        self.queued -= 1;
        Ok(Some(self.begin_turn(now).await?))
    }

    /// Wake on the target host after a restore: emit `Resumed` (the clock fix-up
    /// trigger) and reopen the gate so the queued backlog can replay. Any turn
    /// that was in flight at snapshot time is still in flight here — the
    /// snapshot froze it mid-turn — so `resume` does not touch `in_flight`.
    pub async fn resume(&mut self, now: Timestamp) -> Result<(), GuestError> {
        self.chan.send(GuestToHost::Resumed { ts: now }).await?;
        self.gated = false;
        Ok(())
    }

    /// Assign the next turn id, mark it in flight, and emit `TurnStarted`. The
    /// shared core of [`start_turn`](Self::start_turn) and
    /// [`replay_next`](Self::replay_next); callers enforce the gate and the
    /// no-overlap precondition.
    async fn begin_turn(&mut self, now: Timestamp) -> Result<TurnId, GuestError> {
        let turn_id = self.next_turn;
        self.next_turn = self.next_turn.next();
        self.in_flight = Some(turn_id);
        self.chan
            .send(GuestToHost::TurnStarted { turn_id, ts: now })
            .await?;
        Ok(turn_id)
    }

    /// End the in-flight turn: send `TurnEnded` and clear it.
    pub async fn end_turn(&mut self, now: Timestamp) -> Result<(), GuestError> {
        let turn_id = self.in_flight.ok_or(GuestError::NoTurnInFlight)?;
        self.chan
            .send(GuestToHost::TurnEnded { turn_id, ts: now })
            .await?;
        self.in_flight = None;
        Ok(())
    }

    /// React to one message from hostd. `now` is the guest's clock, used by the
    /// turn-driving messages ([`RunTurn`](HostToGuest::RunTurn)); the control
    /// messages ignore it.
    pub async fn handle(&mut self, msg: HostToGuest, now: Timestamp) -> Result<(), GuestError> {
        match msg {
            HostToGuest::RunTurn { turn_id } => self.run_turn(turn_id, now).await.map(|_| ()),
            HostToGuest::DrainRequest { deadline: _ } => {
                // Close the gate, then report what's still running. `None` means
                // the app layer is quiescent; `Some` means hostd must wait for
                // that turn (the race rule — the turn wins).
                self.gated = true;
                self.chan
                    .send(GuestToHost::DrainAck {
                        in_flight: self.in_flight,
                    })
                    .await?;
                Ok(())
            }
            HostToGuest::DrainCancel => {
                // Migration aborted: reopen the gate. The queued backlog stays
                // put and replays on this same host (via `replay_next`) — the
                // turns were deferred, not dropped.
                self.gated = false;
                Ok(())
            }
            HostToGuest::Secrets { env } => {
                // Late secret update (e.g. a rotated key); idempotent.
                self.secrets = env;
                Ok(())
            }
            HostToGuest::Ping => {
                self.chan.send(GuestToHost::Pong).await?;
                Ok(())
            }
            HostToGuest::Pong => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

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

    #[tokio::test]
    async fn handshake_sends_hello_and_stores_secrets() {
        let mut g = guest();
        let mut env = BTreeMap::new();
        env.insert("ANTHROPIC_API_KEY".to_owned(), "sk-x".to_owned());
        g.chan
            .push_inbound(HostToGuest::Secrets { env: env.clone() });

        g.handshake().await.expect("handshake");

        assert_eq!(g.secrets(), &env);
        assert!(matches!(
            g.chan.sent().as_slice(),
            [GuestToHost::Hello { .. }]
        ));
    }

    #[tokio::test]
    async fn handshake_rejects_non_secrets_reply() {
        let mut g = guest();
        g.chan.push_inbound(HostToGuest::Ping);
        let err = g.handshake().await.expect_err("must reject");
        assert!(matches!(err, GuestError::UnexpectedHandshake));
    }

    #[tokio::test]
    async fn turns_are_monotonic_and_emit_signals() {
        let mut g = guest();
        let StartOutcome::Started(t0) = g.start_turn(ts(1)).await.expect("start") else {
            panic!("should start");
        };
        g.end_turn(ts(2)).await.expect("end");
        let StartOutcome::Started(t1) = g.start_turn(ts(3)).await.expect("start") else {
            panic!("should start");
        };
        g.end_turn(ts(4)).await.expect("end");

        assert_eq!(t0, TurnId::FIRST);
        assert_eq!(t1, TurnId::FIRST.next());
        assert!(matches!(
            g.chan.sent().as_slice(),
            [
                GuestToHost::TurnStarted { .. },
                GuestToHost::TurnEnded { .. },
                GuestToHost::TurnStarted { .. },
                GuestToHost::TurnEnded { .. },
            ]
        ));
    }

    #[tokio::test]
    async fn host_driven_turn_echoes_its_id_and_clears() {
        let mut g = guest();
        let id = TurnId::from_u64(42);
        let outcome = g.run_turn(id, ts(1)).await.expect("run");
        assert_eq!(outcome, StartOutcome::Started(id));
        // Emits started+ended under the host's id; nothing left in flight.
        assert!(g.in_flight().is_none());
        assert!(matches!(
            g.chan.sent().as_slice(),
            [
                GuestToHost::TurnStarted { turn_id: a, .. },
                GuestToHost::TurnEnded { turn_id: b, .. },
            ] if *a == id && *b == id
        ));
    }

    #[tokio::test]
    async fn host_driven_turn_is_deferred_while_gated() {
        let mut g = guest();
        g.handle(
            HostToGuest::DrainRequest {
                deadline: Duration::from_secs(5),
            },
            ts(0),
        )
        .await
        .expect("drain");
        // A RunTurn arriving after the gate closes is queued, not run on the wire.
        let outcome = g.run_turn(TurnId::from_u64(7), ts(1)).await.expect("gated");
        assert_eq!(outcome, StartOutcome::Queued);
        assert_eq!(g.queued_turns(), 1);
        assert!(matches!(
            g.chan.sent().as_slice(),
            [GuestToHost::DrainAck { in_flight: None }]
        ));
    }

    #[tokio::test]
    async fn starting_a_turn_while_one_runs_is_rejected() {
        let mut g = guest();
        g.start_turn(ts(1)).await.expect("start");
        let err = g.start_turn(ts(2)).await.expect_err("must reject");
        assert!(matches!(err, GuestError::TurnInProgress { .. }));
    }

    #[tokio::test]
    async fn ending_with_no_turn_is_rejected() {
        let mut g = guest();
        let err = g.end_turn(ts(1)).await.expect_err("must reject");
        assert!(matches!(err, GuestError::NoTurnInFlight));
    }

    #[tokio::test]
    async fn drain_while_idle_acks_none_and_gates_new_turns() {
        let mut g = guest();
        g.handle(
            HostToGuest::DrainRequest {
                deadline: Duration::from_secs(5),
            },
            ts(0),
        )
        .await
        .expect("drain");

        assert!(g.is_gated());
        assert!(matches!(
            g.chan.sent().as_slice(),
            [GuestToHost::DrainAck { in_flight: None }]
        ));
        // A turn arriving after the gate closes is queued, not started.
        assert_eq!(
            g.start_turn(ts(1)).await.expect("gated start"),
            StartOutcome::Queued
        );
        assert_eq!(g.queued_turns(), 1);
        // Nothing new was sent for the queued turn.
        assert_eq!(g.chan.sent().len(), 1);
    }

    #[tokio::test]
    async fn queued_turns_replay_after_resume_with_continuing_ids() {
        let mut g = guest();
        // One turn runs and completes before the drain.
        let StartOutcome::Started(t0) = g.start_turn(ts(1)).await.expect("start") else {
            panic!("should start");
        };
        g.end_turn(ts(2)).await.expect("end");

        // Drain gates the gate; two turns arrive and are queued.
        g.handle(
            HostToGuest::DrainRequest {
                deadline: Duration::from_secs(5),
            },
            ts(0),
        )
        .await
        .expect("drain");
        assert_eq!(g.start_turn(ts(3)).await.expect("q"), StartOutcome::Queued);
        assert_eq!(g.start_turn(ts(4)).await.expect("q"), StartOutcome::Queued);
        assert_eq!(g.queued_turns(), 2);

        // Replaying while gated is refused — the gate must reopen first.
        assert!(matches!(
            g.replay_next(ts(5)).await.expect_err("gated"),
            GuestError::GatedReplay
        ));

        // Resume on the target: emits Resumed, reopens the gate.
        g.resume(ts(6)).await.expect("resume");
        assert!(!g.is_gated());
        assert!(matches!(
            g.chan.sent().last(),
            Some(GuestToHost::Resumed { .. })
        ));

        // The backlog replays in order; ids continue monotonically from t0.
        let r0 = g.replay_next(ts(7)).await.expect("replay").expect("one");
        g.end_turn(ts(8)).await.expect("end");
        let r1 = g.replay_next(ts(9)).await.expect("replay").expect("two");
        g.end_turn(ts(10)).await.expect("end");
        assert_eq!(g.replay_next(ts(11)).await.expect("drained"), None);

        assert_eq!(r0, t0.next());
        assert_eq!(r1, t0.next().next());
        assert_eq!(g.queued_turns(), 0);
    }

    #[tokio::test]
    async fn drain_cancel_lets_the_backlog_replay_on_the_same_host() {
        let mut g = guest();
        g.handle(
            HostToGuest::DrainRequest {
                deadline: Duration::from_secs(5),
            },
            ts(0),
        )
        .await
        .expect("drain");
        assert_eq!(g.start_turn(ts(1)).await.expect("q"), StartOutcome::Queued);

        // Cancel reopens the gate without losing the queued turn.
        g.handle(HostToGuest::DrainCancel, ts(0))
            .await
            .expect("cancel");
        assert_eq!(g.queued_turns(), 1);
        let replayed = g.replay_next(ts(2)).await.expect("replay").expect("one");
        assert_eq!(replayed, TurnId::FIRST);
        assert_eq!(g.queued_turns(), 0);
    }

    #[tokio::test]
    async fn replay_is_refused_while_a_turn_is_in_flight() {
        let mut g = guest();
        g.handle(
            HostToGuest::DrainRequest {
                deadline: Duration::from_secs(5),
            },
            ts(0),
        )
        .await
        .expect("drain");
        g.start_turn(ts(1)).await.expect("q"); // queued
        g.handle(HostToGuest::DrainCancel, ts(0))
            .await
            .expect("cancel");

        // Start a fresh turn, then a replay must be refused until it ends.
        g.start_turn(ts(2)).await.expect("start");
        assert!(matches!(
            g.replay_next(ts(3)).await.expect_err("in flight"),
            GuestError::TurnInProgress { .. }
        ));
    }

    #[tokio::test]
    async fn drain_while_busy_acks_the_in_flight_turn() {
        let mut g = guest();
        let StartOutcome::Started(turn) = g.start_turn(ts(1)).await.expect("start") else {
            panic!("should start");
        };
        g.handle(
            HostToGuest::DrainRequest {
                deadline: Duration::from_secs(5),
            },
            ts(0),
        )
        .await
        .expect("drain");

        // The race rule: the running turn is reported, never cut.
        assert!(matches!(
            g.chan.sent().last(),
            Some(GuestToHost::DrainAck { in_flight: Some(t) }) if *t == turn
        ));
    }

    #[tokio::test]
    async fn drain_cancel_reopens_the_gate() {
        let mut g = guest();
        g.handle(
            HostToGuest::DrainRequest {
                deadline: Duration::from_secs(5),
            },
            ts(0),
        )
        .await
        .expect("drain");
        assert!(g.is_gated());

        g.handle(HostToGuest::DrainCancel, ts(0))
            .await
            .expect("cancel");
        assert!(!g.is_gated());
        // Turns flow again.
        assert!(matches!(
            g.start_turn(ts(9)).await.expect("start"),
            StartOutcome::Started(_)
        ));
    }

    #[tokio::test]
    async fn ping_is_answered_with_pong() {
        let mut g = guest();
        g.handle(HostToGuest::Ping, ts(0)).await.expect("ping");
        assert!(matches!(g.chan.sent().as_slice(), [GuestToHost::Pong]));
    }
}
