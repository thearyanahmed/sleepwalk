//! Driver tests: the migration walks the FSM in order, the race rule aborts
//! before snapshotting, and effect failures propagate.

use std::time::Duration;

use proto::fsm::Migration;
use proto::{HostId, TurnId, VmId};
use rebalancer::{AbortReason, DrainOutcome, ExecError, MigrationError, MigrationOutcome};
use rebalancer::{PseudoExecutor, drive};

const DEADLINE: Duration = Duration::from_secs(5);

fn migration() -> Migration<proto::fsm::state::Intent> {
    Migration::intent(VmId::new(), HostId::new("host-a"), HostId::new("host-b"))
}

#[tokio::test]
async fn quiescent_drain_runs_the_full_migration_in_order() {
    let exec = PseudoExecutor::new(); // drain → Quiescent
    let outcome = drive(migration(), &exec, DEADLINE)
        .await
        .expect("migration drives cleanly");

    assert_eq!(outcome, MigrationOutcome::Completed);
    assert_eq!(
        exec.calls(),
        [
            "request_drain",
            "snapshot",
            "transfer",
            "restore",
            "cutover",
            "cleanup",
        ]
    );
}

#[tokio::test]
async fn busy_drain_aborts_before_snapshotting() {
    let exec = PseudoExecutor::with_drain(DrainOutcome::Busy {
        in_flight: Some(TurnId::from_u64(3)),
    });
    let outcome = drive(migration(), &exec, DEADLINE)
        .await
        .expect("abort is not an error");

    // Race rule: the turn won. The VM stays put; nothing was snapshotted.
    assert_eq!(
        outcome,
        MigrationOutcome::Aborted(AbortReason::NotQuiescent)
    );
    assert_eq!(exec.calls(), ["request_drain", "cancel_drain"]);
}

#[tokio::test]
async fn drain_failure_propagates_and_stops_the_migration() {
    let exec = PseudoExecutor::new();
    exec.fail_on("request_drain", "control plane unreachable");
    let err = drive(migration(), &exec, DEADLINE)
        .await
        .expect_err("must surface the executor error");

    assert!(matches!(
        err,
        MigrationError::Executor(ExecError {
            op: "request_drain",
            ..
        })
    ));
    // Nothing past the failed drain ran.
    assert_eq!(exec.calls(), ["request_drain"]);
}

#[tokio::test]
async fn transfer_failure_after_snapshot_propagates_without_rollback() {
    // Past snapshotting there is no abort (the typestate forbids it); a failure
    // surfaces as an error for hostd to fail over, not a silent rollback.
    let exec = PseudoExecutor::new();
    exec.fail_on("transfer", "peer reset");
    let err = drive(migration(), &exec, DEADLINE)
        .await
        .expect_err("transfer failure must surface");

    assert!(matches!(
        err,
        MigrationError::Executor(ExecError { op: "transfer", .. })
    ));
    // The snapshot was taken; the migration stopped at transfer.
    assert_eq!(exec.calls(), ["request_drain", "snapshot", "transfer"]);
}
