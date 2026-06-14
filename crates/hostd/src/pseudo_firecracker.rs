//! A recording, fault-injecting stand-in for Firecracker, used in tests.
//!
//! [`PseudoFirecracker`] implements [`FirecrackerApi`](crate::firecracker::FirecrackerApi)
//! without a real VM, so the lifecycle logic in [`crate::vm`] is testable
//! without `/dev/kvm`. It records the ordered sequence of operations it received
//! (so a test can assert hostd issued exactly `boot, pause, resume, shutdown`)
//! and can be primed to fail the next call to a given operation, to exercise
//! error paths.

use std::sync::Mutex;

use crate::firecracker::{
    BootSource, Drive, FirecrackerApi, FirecrackerError, MachineConfig, SnapshotSource,
    SnapshotTarget,
};

/// A fake Firecracker that records calls and can inject failures.
#[derive(Debug, Default)]
pub struct PseudoFirecracker {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    calls: Vec<&'static str>,
    fail_next: Option<(&'static str, FailKind)>,
}

#[derive(Debug, Clone, Copy)]
enum FailKind {
    Rejected,
    Unreachable,
}

impl PseudoFirecracker {
    /// A fresh fake that succeeds every call.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Prime the fake so the next call to `op` is rejected by Firecracker.
    pub fn reject_next(&self, op: &'static str) {
        self.set_fail(op, FailKind::Rejected);
    }

    /// Prime the fake so the next call to `op` fails as unreachable.
    pub fn unreachable_next(&self, op: &'static str) {
        self.set_fail(op, FailKind::Unreachable);
    }

    /// The ordered operations the fake has received.
    #[must_use]
    pub fn calls(&self) -> Vec<&'static str> {
        self.lock().calls.clone()
    }

    fn set_fail(&self, op: &'static str, kind: FailKind) {
        self.lock().fail_next = Some((op, kind));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // The only panic source is a poisoned lock, which means a prior test
        // thread panicked while holding it — surfacing that is correct here.
        #[allow(clippy::unwrap_used)]
        self.state.lock().unwrap()
    }

    /// Record `op`; honor a primed failure for it.
    fn record(&self, op: &'static str) -> Result<(), FirecrackerError> {
        let mut st = self.lock();
        st.calls.push(op);
        if let Some((failed_op, kind)) = st.fail_next
            && failed_op == op
        {
            st.fail_next = None;
            let detail = "injected by PseudoFirecracker".to_owned();
            return Err(match kind {
                FailKind::Rejected => FirecrackerError::Rejected { op, detail },
                FailKind::Unreachable => FirecrackerError::Unreachable { op, detail },
            });
        }
        Ok(())
    }
}

impl FirecrackerApi for PseudoFirecracker {
    async fn configure_machine(&self, _cfg: MachineConfig) -> Result<(), FirecrackerError> {
        self.record("configure_machine")
    }
    async fn configure_boot_source(&self, _src: BootSource) -> Result<(), FirecrackerError> {
        self.record("configure_boot_source")
    }
    async fn configure_drive(&self, _drive: Drive) -> Result<(), FirecrackerError> {
        self.record("configure_drive")
    }
    async fn boot(&self) -> Result<(), FirecrackerError> {
        self.record("boot")
    }
    async fn pause(&self) -> Result<(), FirecrackerError> {
        self.record("pause")
    }
    async fn resume(&self) -> Result<(), FirecrackerError> {
        self.record("resume")
    }
    async fn shutdown(&self) -> Result<(), FirecrackerError> {
        self.record("shutdown")
    }
    async fn create_snapshot(&self, _target: SnapshotTarget) -> Result<(), FirecrackerError> {
        self.record("create_snapshot")
    }
    async fn load_snapshot(&self, _source: SnapshotSource) -> Result<(), FirecrackerError> {
        self.record("load_snapshot")
    }
}
