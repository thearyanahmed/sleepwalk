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
//! is never cut short; turns that arrive *after* the gate closes are refused
//! ([`StartOutcome::Gated`]) and replay after resume.

use std::collections::BTreeMap;

use proto::{GuestToHost, GuestdVersion, HostToGuest, Timestamp, TurnId, VmId};
use thiserror::Error;

use crate::channel::{ChannelError, GuestChannel};

/// What happened when the workload tried to start a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartOutcome {
    /// The turn started; here is its id (`TurnStarted` was sent).
    Started(TurnId),
    /// The drain gate is closed — the turn was refused and should be replayed
    /// after the VM resumes on the target host. Nothing was sent.
    Gated,
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
    /// True once a `DrainRequest` has closed the gate; new turns are refused.
    gated: bool,
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
        }
    }

    /// The turn in flight, if any.
    #[must_use]
    pub fn in_flight(&self) -> Option<TurnId> {
        self.in_flight
    }

    /// Whether the drain gate is closed (new turns refused).
    #[must_use]
    pub fn is_gated(&self) -> bool {
        self.gated
    }

    /// The secrets received at boot (the env handed to the workload).
    #[must_use]
    pub fn secrets(&self) -> &BTreeMap<String, String> {
        &self.secrets
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

    /// Start a turn. Refused with [`StartOutcome::Gated`] if the drain gate is
    /// closed; otherwise assigns the next id, marks it in flight, and sends
    /// `TurnStarted`.
    pub async fn start_turn(&mut self, now: Timestamp) -> Result<StartOutcome, GuestError> {
        if let Some(in_flight) = self.in_flight {
            return Err(GuestError::TurnInProgress { in_flight });
        }
        if self.gated {
            return Ok(StartOutcome::Gated);
        }
        let turn_id = self.next_turn;
        self.next_turn = self.next_turn.next();
        self.in_flight = Some(turn_id);
        self.chan
            .send(GuestToHost::TurnStarted { turn_id, ts: now })
            .await?;
        Ok(StartOutcome::Started(turn_id))
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

    /// React to one message from hostd.
    pub async fn handle(&mut self, msg: HostToGuest) -> Result<(), GuestError> {
        match msg {
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
                // Migration aborted: reopen the gate, release queued turns.
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
        g.handle(HostToGuest::DrainRequest {
            deadline: Duration::from_secs(5),
        })
        .await
        .expect("drain");

        assert!(g.is_gated());
        assert!(matches!(
            g.chan.sent().as_slice(),
            [GuestToHost::DrainAck { in_flight: None }]
        ));
        // A turn arriving after the gate closes is refused, not started.
        assert_eq!(
            g.start_turn(ts(1)).await.expect("gated start"),
            StartOutcome::Gated
        );
        // Nothing new was sent for the gated turn.
        assert_eq!(g.chan.sent().len(), 1);
    }

    #[tokio::test]
    async fn drain_while_busy_acks_the_in_flight_turn() {
        let mut g = guest();
        let StartOutcome::Started(turn) = g.start_turn(ts(1)).await.expect("start") else {
            panic!("should start");
        };
        g.handle(HostToGuest::DrainRequest {
            deadline: Duration::from_secs(5),
        })
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
        g.handle(HostToGuest::DrainRequest {
            deadline: Duration::from_secs(5),
        })
        .await
        .expect("drain");
        assert!(g.is_gated());

        g.handle(HostToGuest::DrainCancel).await.expect("cancel");
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
        g.handle(HostToGuest::Ping).await.expect("ping");
        assert!(matches!(g.chan.sent().as_slice(), [GuestToHost::Pong]));
    }
}
