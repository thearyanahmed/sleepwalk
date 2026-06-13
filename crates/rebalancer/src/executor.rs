//! The migration executor port.
//!
//! [`MigrationExecutor`] is the seam between the migration *decision* (the FSM
//! driver in [`crate::driver`]) and the *effects* (talking to hostd: drain the
//! guest, snapshot, stream the snapshot, restore on the target, cut over, clean
//! up). The driver owns the legal order; the executor does the work. The real
//! implementation drives hostd over the control plane; tests use
//! [`crate::pseudo_executor::PseudoExecutor`].

use std::future::Future;
use std::time::Duration;

use proto::{HostId, TurnId, VmId};
use thiserror::Error;

/// The result of asking the source host to drain the VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainOutcome {
    /// All quiescence layers are satisfied — safe to snapshot.
    Quiescent,
    /// The drain deadline elapsed without quiescence (a turn was still in
    /// flight, or an infra/storage layer was not quiet). By the race rule the
    /// turn wins: the migration must abort.
    Busy {
        /// The turn still running at the deadline, if the app layer was the
        /// hold-out.
        in_flight: Option<TurnId>,
    },
}

/// An error from one executor effect, carrying enough to debug from a log line.
#[derive(Debug, Error)]
#[error("migration executor failed at {op} for vm {vm}: {detail}")]
pub struct ExecError {
    /// The effect that failed (`snapshot`, `transfer`, …).
    pub op: &'static str,
    /// The VM the effect was for.
    pub vm: VmId,
    /// What went wrong.
    pub detail: String,
}

/// The effects the migration driver invokes, in the order the FSM dictates.
///
/// `request_drain` is the only one that returns a decision; the rest either
/// succeed or fail. Hosts are passed by value (cloned by the driver) so the
/// returned futures borrow nothing.
pub trait MigrationExecutor {
    /// Ask the source host to gate new turns and report quiescence, waiting up
    /// to `deadline`.
    fn request_drain(
        &self,
        vm: VmId,
        deadline: Duration,
    ) -> impl Future<Output = Result<DrainOutcome, ExecError>> + Send;

    /// Un-gate the guest after an aborted drain (sends `DrainCancel`).
    fn cancel_drain(&self, vm: VmId) -> impl Future<Output = Result<(), ExecError>> + Send;

    /// Pause the VM and write its snapshot on the source host.
    fn snapshot(&self, vm: VmId) -> impl Future<Output = Result<(), ExecError>> + Send;

    /// Stream the snapshot from the source to `to`.
    fn transfer(&self, vm: VmId, to: HostId) -> impl Future<Output = Result<(), ExecError>> + Send;

    /// Restore the VM from the snapshot on `to` (UFFD lazy restore).
    fn restore(&self, vm: VmId, to: HostId) -> impl Future<Output = Result<(), ExecError>> + Send;

    /// Switch authority to `to`: re-plumb the tap, release queued turns.
    fn cutover(&self, vm: VmId, to: HostId) -> impl Future<Output = Result<(), ExecError>> + Send;

    /// Tear down source-side state on `from` (delete the snapshot dir).
    fn cleanup(&self, vm: VmId, from: HostId)
    -> impl Future<Output = Result<(), ExecError>> + Send;
}
