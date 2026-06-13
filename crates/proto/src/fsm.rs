//! The migration state machine, rebalancer-owned.
//!
//! Two representations, deliberately:
//!
//! - [`Migration<S>`] is a **typestate**. The state is a type parameter, so the
//!   only methods that exist on a value are the transitions legal from its
//!   state. `snapshot()` exists on `Migration<Quiescent>` and nowhere else, so
//!   snapshotting before quiescence is a *compile* error, not a runtime check.
//!   This crate models the legal shape; the rebalancer does the real work
//!   (draining, copying bytes) between transitions and calls them on success.
//! - [`MigrationState`] is a plain enum for the things that need a value at
//!   runtime: structured-log transcripts, the Grafana FSM gauge, the `/metrics`
//!   endpoint.
//!
//! The forward path is
//! `Intent → Draining → Quiescent → Snapshotting → Transferring → Restoring →
//! CutOver → Cleanup`. [`abort`][Migration::abort] is available only *before*
//! snapshotting; once memory has been dumped a migration runs to completion or
//! fails over to resume-on-source, so no `abort` method exists past that point.

use std::marker::PhantomData;

use serde::{Deserialize, Serialize};

use crate::ids::{HostId, VmId};

/// Runtime-inspectable migration state, for logs, transcripts, and metrics.
///
/// The compile-time guarantees live in [`Migration<S>`]; this is its shadow for
/// places that need to *store* or *serialize* a state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MigrationState {
    /// No migration in progress for this VM (the resting state).
    Stable,
    /// The rebalancer has decided to move this VM but has done nothing yet.
    Intent,
    /// Drain requested; new turns are being gated, waiting for quiescence.
    Draining,
    /// All three quiescence layers satisfied; safe to snapshot.
    Quiescent,
    /// Pausing the VM and writing the snapshot. **Past the point of no abort.**
    Snapshotting,
    /// Streaming the snapshot to the target host.
    Transferring,
    /// Target host is restoring (UFFD lazy restore) from the snapshot.
    Restoring,
    /// Switching authority to the target: re-plumb the tap, release queued turns.
    CutOver,
    /// Tearing down source-side state (snapshot dir, FC process).
    Cleanup,
    /// Migration was aborted before snapshotting; the VM stays put on the source.
    Aborted,
}

mod sealed {
    pub trait Sealed {}
}

/// Marker trait for the typestate phases. Sealed: only the types in [`state`]
/// implement it.
pub trait Phase: sealed::Sealed {
    /// The runtime tag for this phase.
    const STATE: MigrationState;
}

/// Marker trait for phases from which a migration can still be safely aborted —
/// everything *before* [`Snapshotting`][state::Snapshotting]. Implemented for
/// [`Intent`][state::Intent], [`Draining`][state::Draining], and
/// [`Quiescent`][state::Quiescent] only, so [`abort`][Migration::abort] is a
/// compile error elsewhere.
pub trait Abortable: Phase {}

/// Zero-sized marker types, one per migration phase. These are type-level tags;
/// they hold no data and are never constructed as values.
pub mod state {
    use super::{Abortable, MigrationState, Phase, sealed};

    /// Define a phase marker type that implements [`Phase`].
    macro_rules! phase {
        ($(#[$m:meta])* $name:ident => $variant:ident) => {
            $(#[$m])*
            #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
            pub enum $name {}
            impl sealed::Sealed for $name {}
            impl Phase for $name {
                const STATE: MigrationState = MigrationState::$variant;
            }
        };
    }

    phase!(/// Decided, not yet acted on.
        Intent => Intent);
    phase!(/// Gating new turns, awaiting quiescence.
        Draining => Draining);
    phase!(/// Verified quiescent; ready to snapshot.
        Quiescent => Quiescent);
    phase!(/// Pausing + writing the snapshot.
        Snapshotting => Snapshotting);
    phase!(/// Streaming the snapshot to the target.
        Transferring => Transferring);
    phase!(/// Target restoring from the snapshot.
        Restoring => Restoring);
    phase!(/// Switching authority to the target.
        CutOver => CutOver);
    phase!(/// Tearing down source-side state.
        Cleanup => Cleanup);
    phase!(/// Aborted before snapshotting; VM stays on the source.
        Stable => Aborted);

    impl Abortable for Intent {}
    impl Abortable for Draining {}
    impl Abortable for Quiescent {}
}

/// A migration of one VM from a source host to a target host, in phase `S`.
///
/// Construct with [`Migration::intent`] and walk it forward one transition at a
/// time. Each transition consumes `self`, so a stale handle to a past phase
/// cannot be used by mistake.
#[derive(Debug)]
pub struct Migration<S: Phase> {
    vm: VmId,
    from: HostId,
    to: HostId,
    _phase: PhantomData<S>,
}

impl<S: Phase> Migration<S> {
    /// The VM being moved.
    #[must_use]
    pub const fn vm(&self) -> VmId {
        self.vm
    }

    /// The source host.
    #[must_use]
    pub fn from(&self) -> &HostId {
        &self.from
    }

    /// The target host.
    #[must_use]
    pub fn to(&self) -> &HostId {
        &self.to
    }

    /// This migration's current phase as a runtime value, for logs and metrics.
    #[must_use]
    pub const fn state(&self) -> MigrationState {
        S::STATE
    }

    /// Re-tag this migration as phase `T`, carrying its data forward. Private:
    /// the public surface is the named transitions below, which fix the legal
    /// order at the type level.
    fn advance<T: Phase>(self) -> Migration<T> {
        Migration {
            vm: self.vm,
            from: self.from,
            to: self.to,
            _phase: PhantomData,
        }
    }
}

impl Migration<state::Intent> {
    /// Begin a migration: the rebalancer has chosen to move `vm` from `from` to
    /// `to`. Nothing has happened to the VM yet.
    #[must_use]
    pub fn intent(vm: VmId, from: HostId, to: HostId) -> Self {
        Migration {
            vm,
            from,
            to,
            _phase: PhantomData,
        }
    }

    /// Issue the drain request and begin gating new turns.
    #[must_use]
    pub fn drain(self) -> Migration<state::Draining> {
        self.advance()
    }
}

impl Migration<state::Draining> {
    /// All three quiescence layers (app, infra, storage) are satisfied.
    #[must_use]
    pub fn quiescent(self) -> Migration<state::Quiescent> {
        self.advance()
    }
}

impl Migration<state::Quiescent> {
    /// Pause the VM and write the snapshot. **No abort exists past this point.**
    #[must_use]
    pub fn snapshot(self) -> Migration<state::Snapshotting> {
        self.advance()
    }
}

impl Migration<state::Snapshotting> {
    /// Begin streaming the snapshot to the target host.
    #[must_use]
    pub fn transfer(self) -> Migration<state::Transferring> {
        self.advance()
    }
}

impl Migration<state::Transferring> {
    /// The target begins restoring from the received snapshot.
    #[must_use]
    pub fn restore(self) -> Migration<state::Restoring> {
        self.advance()
    }
}

impl Migration<state::Restoring> {
    /// Switch authority to the target host (re-plumb tap, release queued turns).
    #[must_use]
    pub fn cutover(self) -> Migration<state::CutOver> {
        self.advance()
    }
}

impl Migration<state::CutOver> {
    /// Tear down source-side state. The terminal success phase.
    #[must_use]
    pub fn cleanup(self) -> Migration<state::Cleanup> {
        self.advance()
    }
}

impl<S: Abortable> Migration<S> {
    /// Abort the migration and leave the VM on the source host. Available only
    /// before [`snapshot`][Migration::snapshot]; calling it on a later phase is
    /// a compile error.
    ///
    /// ```compile_fail
    /// # use proto::fsm::Migration;
    /// # use proto::ids::{HostId, VmId};
    /// let m = Migration::intent(VmId::new(), HostId::new("a"), HostId::new("b"))
    ///     .drain()
    ///     .quiescent()
    ///     .snapshot(); // now in Snapshotting
    /// m.abort(); // ERROR: no method `abort` once snapshotting has begun
    /// ```
    #[must_use]
    pub fn abort(self) -> Migration<state::Stable> {
        self.advance()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Migration<state::Intent> {
        Migration::intent(
            VmId::from_uuid(uuid::Uuid::nil()),
            HostId::new("host-a"),
            HostId::new("host-b"),
        )
    }

    /// The full happy path walks every phase and ends in `Cleanup`, preserving
    /// the VM and host identity the whole way.
    #[test]
    fn forward_path_reaches_cleanup() {
        let m = fixture();
        let vm = m.vm();
        assert_eq!(m.state(), MigrationState::Intent);

        let m = m.drain();
        assert_eq!(m.state(), MigrationState::Draining);
        let m = m.quiescent();
        assert_eq!(m.state(), MigrationState::Quiescent);
        let m = m.snapshot();
        assert_eq!(m.state(), MigrationState::Snapshotting);
        let m = m.transfer();
        assert_eq!(m.state(), MigrationState::Transferring);
        let m = m.restore();
        assert_eq!(m.state(), MigrationState::Restoring);
        let m = m.cutover();
        assert_eq!(m.state(), MigrationState::CutOver);
        let m = m.cleanup();
        assert_eq!(m.state(), MigrationState::Cleanup);

        assert_eq!(m.vm(), vm);
        assert_eq!(m.from().as_str(), "host-a");
        assert_eq!(m.to().as_str(), "host-b");
    }

    /// Aborting from each pre-snapshot phase lands in `Stable` (Aborted tag).
    #[test]
    fn abort_before_snapshot_returns_to_stable() {
        assert_eq!(fixture().abort().state(), MigrationState::Aborted);
        assert_eq!(fixture().drain().abort().state(), MigrationState::Aborted);
        assert_eq!(
            fixture().drain().quiescent().abort().state(),
            MigrationState::Aborted
        );
    }

    /// `MigrationState` round-trips through JSON (it goes into transcripts).
    #[test]
    fn migration_state_round_trips() {
        for st in [
            MigrationState::Stable,
            MigrationState::Intent,
            MigrationState::Draining,
            MigrationState::Quiescent,
            MigrationState::Snapshotting,
            MigrationState::Transferring,
            MigrationState::Restoring,
            MigrationState::CutOver,
            MigrationState::Cleanup,
            MigrationState::Aborted,
        ] {
            let json = serde_json::to_string(&st).expect("serialize");
            let back: MigrationState = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(st, back);
        }
    }
}
