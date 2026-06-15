//! The migration driver.
//!
//! [`drive`] walks one migration from `Intent` to `Cleanup`, calling the
//! [`MigrationExecutor`] at each step. It is the place the race rule and the
//! point-of-no-return are enforced — and both are enforced *by the type system*,
//! via proto's [`Migration`] typestate:
//!
//! - The drain gate: if [`request_drain`](MigrationExecutor::request_drain)
//!   comes back [`DrainOutcome::Busy`], the migration aborts to `Stable` and the
//!   turn is never cut. `abort` is only callable here because the migration is
//!   still pre-snapshot.
//! - The point of no return: once [`snapshot`](MigrationExecutor::snapshot) has
//!   run, the typestate offers no `abort` — a later executor failure propagates
//!   as an error (its handling, fail-over to resume-on-source, is a later
//!   slice), it cannot silently roll back.

use std::time::Duration;

use proto::fsm::{Migration, state};
use thiserror::Error;

use crate::executor::{DrainOutcome, ExecError, MigrationExecutor};

/// How a driven migration ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationOutcome {
    /// The VM was relocated and source-side state cleaned up.
    Completed,
    /// The migration aborted before snapshotting; the VM stayed on the source.
    Aborted(AbortReason),
}

/// Why a migration aborted (only possible before snapshotting).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbortReason {
    /// The drain did not reach quiescence before the deadline — the turn won.
    NotQuiescent,
}

/// A failure that stopped a migration partway.
#[derive(Debug, Error)]
pub enum MigrationError {
    /// An executor effect failed.
    #[error(transparent)]
    Executor(#[from] ExecError),
}

/// Drive a migration from `Intent` to completion (or a clean abort).
///
/// `drain_deadline` bounds how long the source host waits for quiescence before
/// the drain is treated as [`DrainOutcome::Busy`].
pub async fn drive<E: MigrationExecutor>(
    migration: Migration<state::Intent>,
    exec: &E,
    drain_deadline: Duration,
) -> Result<MigrationOutcome, MigrationError> {
    let vm = migration.vm();
    let to = migration.to().clone();
    let from = migration.from().clone();

    // Intent → Draining: gate new turns and ask for quiescence.
    let draining = migration.drain();
    match exec.request_drain(vm, drain_deadline).await? {
        DrainOutcome::Busy { .. } => {
            // Race rule: the in-flight turn wins. Un-gate and stand down.
            exec.cancel_drain(vm).await?;
            let _stable: Migration<state::Stable> = draining.abort();
            Ok(MigrationOutcome::Aborted(AbortReason::NotQuiescent))
        }
        DrainOutcome::Quiescent => {
            // Draining → Quiescent → Snapshotting. Past `snapshot()` there is no
            // `abort` method, so the rest runs to completion or errors out.
            let quiescent = draining.quiescent();
            exec.snapshot(vm).await?;
            let snapshotting = quiescent.snapshot();

            exec.transfer(vm, to.clone()).await?;
            let transferring = snapshotting.transfer();

            exec.restore(vm, to.clone()).await?;
            let restoring = transferring.restore();

            exec.cutover(vm, to.clone()).await?;
            let cutover = restoring.cutover();

            exec.cleanup(vm, from).await?;
            let _cleanup: Migration<state::Cleanup> = cutover.cleanup();

            Ok(MigrationOutcome::Completed)
        }
    }
}

#[cfg(test)]
mod tests {
    use proto::{HostId, VmId};

    use super::*;
    use crate::executor::DrainOutcome;
    use crate::pseudo_executor::PseudoExecutor;

    fn intent() -> Migration<state::Intent> {
        Migration::intent(VmId::new(), HostId::new("a"), HostId::new("b"))
    }

    fn deadline() -> Duration {
        Duration::from_secs(5)
    }

    /// The happy path runs every effect once, in the snapshot-after-drain order.
    #[tokio::test]
    async fn quiescent_drain_completes_in_order() {
        let exec = PseudoExecutor::new(); // drain → Quiescent
        let outcome = drive(intent(), &exec, deadline()).await.expect("drive");
        assert_eq!(outcome, MigrationOutcome::Completed);
        assert_eq!(
            exec.calls(),
            [
                "request_drain",
                "snapshot",
                "transfer",
                "restore",
                "cutover",
                "cleanup"
            ]
        );
    }

    /// The race rule: a busy guest stands the migration down — it cancels the
    /// drain and NEVER snapshots (the in-flight turn is never cut).
    #[tokio::test]
    async fn busy_drain_aborts_without_snapshotting() {
        let exec = PseudoExecutor::with_drain(DrainOutcome::Busy { in_flight: None });
        let outcome = drive(intent(), &exec, deadline()).await.expect("drive");
        assert_eq!(
            outcome,
            MigrationOutcome::Aborted(AbortReason::NotQuiescent)
        );
        assert_eq!(exec.calls(), ["request_drain", "cancel_drain"]);
        assert!(
            !exec.calls().contains(&"snapshot"),
            "must not snapshot a busy guest"
        );
    }

    /// A drain-request failure surfaces and nothing past it runs.
    #[tokio::test]
    async fn drain_request_failure_propagates() {
        let exec = PseudoExecutor::new();
        exec.fail_on("request_drain", "vsock down");
        let err = drive(intent(), &exec, deadline())
            .await
            .expect_err("must fail");
        let MigrationError::Executor(e) = err;
        assert_eq!(e.op, "request_drain");
        assert_eq!(exec.calls(), ["request_drain"]);
    }

    /// A failure at or after `snapshot` is past the point of no return: it
    /// propagates as an error and the driver does NOT roll back (no cancel/abort).
    #[tokio::test]
    async fn snapshot_failure_does_not_roll_back() {
        let exec = PseudoExecutor::new();
        exec.fail_on("snapshot", "disk full");
        let err = drive(intent(), &exec, deadline())
            .await
            .expect_err("must fail");
        let MigrationError::Executor(e) = err;
        assert_eq!(e.op, "snapshot");
        // Snapshot was attempted; nothing rolled back, nothing past it ran.
        assert_eq!(exec.calls(), ["request_drain", "snapshot"]);
        assert!(!exec.calls().contains(&"cancel_drain"));
    }

    /// A post-snapshot transfer failure also just propagates — the VM is already
    /// committed to the move; there is no silent rollback path.
    #[tokio::test]
    async fn transfer_failure_after_snapshot_propagates() {
        let exec = PseudoExecutor::new();
        exec.fail_on("transfer", "connection reset");
        let err = drive(intent(), &exec, deadline())
            .await
            .expect_err("must fail");
        let MigrationError::Executor(e) = err;
        assert_eq!(e.op, "transfer");
        assert_eq!(exec.calls(), ["request_drain", "snapshot", "transfer"]);
    }

    /// A cleanup failure (the very last effect) still surfaces.
    #[tokio::test]
    async fn cleanup_failure_propagates() {
        let exec = PseudoExecutor::new();
        exec.fail_on("cleanup", "stale dir");
        let err = drive(intent(), &exec, deadline())
            .await
            .expect_err("must fail");
        let MigrationError::Executor(e) = err;
        assert_eq!(e.op, "cleanup");
        assert_eq!(
            exec.calls(),
            [
                "request_drain",
                "snapshot",
                "transfer",
                "restore",
                "cutover",
                "cleanup"
            ]
        );
    }
}
