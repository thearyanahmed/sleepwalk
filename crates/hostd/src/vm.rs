//! The per-VM lifecycle orchestrator.
//!
//! [`Vm`] wraps a [`FirecrackerApi`] and tracks the VM's [`RunState`], so the
//! control operations can only be issued in a legal order: you cannot pause a
//! VM that never booted, nor resume one that is already running. Illegal
//! requests are rejected as [`LifecycleError::IllegalTransition`] *before* any
//! Firecracker call is made; a Firecracker failure leaves the state unchanged
//! (a failed `boot` keeps the VM `Created`, not `Running`).
//!
//! This is a runtime check, not a typestate, on purpose: hostd holds VMs in
//! collections and drives them from message handlers, where a value whose type
//! encodes its state would be unwieldy. The migration FSM in `proto`, which is
//! driven as a linear sequence, uses the typestate instead.

use proto::VmId;
use thiserror::Error;

use crate::fc::{FcError, FirecrackerApi};

/// The lifecycle state of a managed VM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    /// Configured but not yet booted.
    Created,
    /// Booted, vCPUs running.
    Running,
    /// Paused (vCPUs stopped); snapshot-eligible.
    Paused,
    /// Shut down; terminal.
    Stopped,
}

/// Why a lifecycle operation could not be carried out.
#[derive(Debug, Error)]
pub enum LifecycleError {
    /// The operation is not legal from the current state.
    #[error("cannot {op} a vm in state {from:?}")]
    IllegalTransition {
        /// The state the VM was in.
        from: RunState,
        /// The operation that was refused.
        op: &'static str,
    },

    /// The operation was legal but Firecracker failed it.
    #[error(transparent)]
    Fc(#[from] FcError),
}

/// One microVM managed by hostd: its identity, its control port, its state.
#[derive(Debug)]
pub struct Vm<F: FirecrackerApi> {
    id: VmId,
    fc: F,
    state: RunState,
}

impl<F: FirecrackerApi> Vm<F> {
    /// Wrap a freshly-configured VM. It starts [`RunState::Created`]; nothing is
    /// sent to Firecracker until [`boot`](Self::boot).
    pub fn new(id: VmId, fc: F) -> Self {
        Self {
            id,
            fc,
            state: RunState::Created,
        }
    }

    /// This VM's id.
    #[must_use]
    pub fn id(&self) -> VmId {
        self.id
    }

    /// This VM's current lifecycle state.
    #[must_use]
    pub fn state(&self) -> RunState {
        self.state
    }

    /// Boot the guest. Legal only from [`RunState::Created`].
    pub async fn boot(&mut self) -> Result<(), LifecycleError> {
        self.require(RunState::Created, "boot")?;
        self.fc.boot().await?;
        self.state = RunState::Running;
        Ok(())
    }

    /// Pause the VM. Legal only from [`RunState::Running`].
    pub async fn pause(&mut self) -> Result<(), LifecycleError> {
        self.require(RunState::Running, "pause")?;
        self.fc.pause().await?;
        self.state = RunState::Paused;
        Ok(())
    }

    /// Resume a paused VM. Legal only from [`RunState::Paused`].
    pub async fn resume(&mut self) -> Result<(), LifecycleError> {
        self.require(RunState::Paused, "resume")?;
        self.fc.resume().await?;
        self.state = RunState::Running;
        Ok(())
    }

    /// Shut the VM down. Legal from [`RunState::Running`] or [`RunState::Paused`].
    pub async fn shutdown(&mut self) -> Result<(), LifecycleError> {
        if !matches!(self.state, RunState::Running | RunState::Paused) {
            return Err(LifecycleError::IllegalTransition {
                from: self.state,
                op: "shutdown",
            });
        }
        self.fc.shutdown().await?;
        self.state = RunState::Stopped;
        Ok(())
    }

    /// Guard: require the VM be in `want` before `op`, else reject.
    fn require(&self, want: RunState, op: &'static str) -> Result<(), LifecycleError> {
        if self.state == want {
            Ok(())
        } else {
            Err(LifecycleError::IllegalTransition {
                from: self.state,
                op,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fc::FakeFc;

    fn vm() -> Vm<FakeFc> {
        Vm::new(VmId::from_uuid(uuid::Uuid::nil()), FakeFc::new())
    }

    #[tokio::test]
    async fn full_lifecycle_issues_ops_in_order() {
        let mut vm = vm();
        assert_eq!(vm.state(), RunState::Created);
        vm.boot().await.expect("boot");
        assert_eq!(vm.state(), RunState::Running);
        vm.pause().await.expect("pause");
        assert_eq!(vm.state(), RunState::Paused);
        vm.resume().await.expect("resume");
        assert_eq!(vm.state(), RunState::Running);
        vm.shutdown().await.expect("shutdown");
        assert_eq!(vm.state(), RunState::Stopped);

        assert_eq!(vm.fc.calls(), ["boot", "pause", "resume", "shutdown"]);
    }

    #[tokio::test]
    async fn pause_before_boot_is_rejected_without_calling_fc() {
        let mut vm = vm();
        let err = vm.pause().await.expect_err("must reject");
        assert!(matches!(
            err,
            LifecycleError::IllegalTransition {
                from: RunState::Created,
                op: "pause"
            }
        ));
        // The guard runs before any Firecracker call.
        assert!(vm.fc.calls().is_empty());
        assert_eq!(vm.state(), RunState::Created);
    }

    #[tokio::test]
    async fn resume_while_running_is_rejected() {
        let mut vm = vm();
        vm.boot().await.expect("boot");
        let err = vm.resume().await.expect_err("must reject");
        assert!(matches!(
            err,
            LifecycleError::IllegalTransition {
                from: RunState::Running,
                ..
            }
        ));
        assert_eq!(vm.state(), RunState::Running);
    }

    #[tokio::test]
    async fn double_shutdown_is_rejected() {
        let mut vm = vm();
        vm.boot().await.expect("boot");
        vm.shutdown().await.expect("shutdown");
        let err = vm.shutdown().await.expect_err("must reject");
        assert!(matches!(
            err,
            LifecycleError::IllegalTransition {
                from: RunState::Stopped,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn firecracker_failure_leaves_state_unchanged() {
        let mut vm = vm();
        vm.fc.reject_next("boot");
        let err = vm.boot().await.expect_err("boot should fail");
        assert!(matches!(
            err,
            LifecycleError::Fc(FcError::Rejected { op: "boot", .. })
        ));
        // boot was attempted but failed: VM stays Created, so a retry is legal.
        assert_eq!(vm.state(), RunState::Created);
        vm.boot().await.expect("retry boot succeeds");
        assert_eq!(vm.state(), RunState::Running);
    }

    #[tokio::test]
    async fn unreachable_firecracker_surfaces_as_fc_error() {
        let mut vm = vm();
        vm.boot().await.expect("boot");
        vm.fc.unreachable_next("pause");
        let err = vm.pause().await.expect_err("pause should fail");
        assert!(matches!(
            err,
            LifecycleError::Fc(FcError::Unreachable { op: "pause", .. })
        ));
        // The op was legal, so the VM is left Running (Firecracker, not the
        // guard, refused it) — caller may retry.
        assert_eq!(vm.state(), RunState::Running);
    }
}
