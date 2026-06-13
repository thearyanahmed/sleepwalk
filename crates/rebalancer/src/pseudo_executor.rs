//! A recording, fault-injecting stand-in for the migration executor, used in
//! tests.
//!
//! [`PseudoExecutor`] implements [`MigrationExecutor`] without touching a real
//! host. It records the ordered effects it received (so a test can assert the
//! driver issued exactly `request_drain, snapshot, transfer, …`), returns a
//! scripted [`DrainOutcome`], and can be primed to fail one named effect.

use std::sync::Mutex;
use std::time::Duration;

use proto::{HostId, VmId};

use crate::executor::{DrainOutcome, ExecError, MigrationExecutor};

/// A fake migration executor for driver tests.
#[derive(Debug)]
pub struct PseudoExecutor {
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    calls: Vec<&'static str>,
    drain: DrainOutcome,
    fail: Option<(&'static str, String)>,
}

impl PseudoExecutor {
    /// A fake whose drain reports [`DrainOutcome::Quiescent`] (the happy path).
    #[must_use]
    pub fn new() -> Self {
        Self::with_drain(DrainOutcome::Quiescent)
    }

    /// A fake whose `request_drain` returns `drain`.
    #[must_use]
    pub fn with_drain(drain: DrainOutcome) -> Self {
        Self {
            state: Mutex::new(State {
                calls: Vec::new(),
                drain,
                fail: None,
            }),
        }
    }

    /// Prime the fake so the next call to effect `op` fails.
    pub fn fail_on(&self, op: &'static str, detail: impl Into<String>) {
        self.lock().fail = Some((op, detail.into()));
    }

    /// The ordered effects the fake has received.
    #[must_use]
    pub fn calls(&self) -> Vec<&'static str> {
        self.lock().calls.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A poisoned lock means a prior test thread panicked holding it;
        // surfacing that is correct in test code.
        #[allow(clippy::unwrap_used)]
        self.state.lock().unwrap()
    }

    /// Record `op` for `vm`; honor a primed failure for it.
    fn record(&self, op: &'static str, vm: VmId) -> Result<(), ExecError> {
        let mut st = self.lock();
        st.calls.push(op);
        if let Some((failed_op, detail)) = &st.fail
            && *failed_op == op
        {
            let detail = detail.clone();
            st.fail = None;
            return Err(ExecError { op, vm, detail });
        }
        Ok(())
    }
}

impl Default for PseudoExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl MigrationExecutor for PseudoExecutor {
    async fn request_drain(
        &self,
        vm: VmId,
        _deadline: Duration,
    ) -> Result<DrainOutcome, ExecError> {
        self.record("request_drain", vm)?;
        Ok(self.lock().drain)
    }

    async fn cancel_drain(&self, vm: VmId) -> Result<(), ExecError> {
        self.record("cancel_drain", vm)
    }

    async fn snapshot(&self, vm: VmId) -> Result<(), ExecError> {
        self.record("snapshot", vm)
    }

    async fn transfer(&self, vm: VmId, _to: HostId) -> Result<(), ExecError> {
        self.record("transfer", vm)
    }

    async fn restore(&self, vm: VmId, _to: HostId) -> Result<(), ExecError> {
        self.record("restore", vm)
    }

    async fn cutover(&self, vm: VmId, _to: HostId) -> Result<(), ExecError> {
        self.record("cutover", vm)
    }

    async fn cleanup(&self, vm: VmId, _from: HostId) -> Result<(), ExecError> {
        self.record("cleanup", vm)
    }
}
