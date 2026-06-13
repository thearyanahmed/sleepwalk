//! The Firecracker control port and a test fake.
//!
//! [`FirecrackerApi`] is the small trait every external Firecracker effect goes
//! through. The real implementation (a later slice) drives Firecracker's HTTP
//! API over the per-VM unix socket; this slice ships only the port and a
//! [`FakeFc`] so the lifecycle logic in [`crate::vm`] is testable without
//! `/dev/kvm`.
//!
//! Each implementor is bound to exactly one VM (it owns that VM's socket path),
//! so the methods take no VM argument.

use std::sync::Mutex;

use thiserror::Error;

/// An error from a single Firecracker control operation.
#[derive(Debug, Error)]
pub enum FcError {
    /// The operation reached Firecracker but it rejected or failed it. `detail`
    /// carries enough to debug from a log line.
    #[error("firecracker rejected {op}: {detail}")]
    Rejected {
        /// The operation that failed (`boot`, `pause`, …).
        op: &'static str,
        /// Firecracker's error detail.
        detail: String,
    },

    /// Firecracker was unreachable (socket gone, process dead, I/O error).
    #[error("firecracker unreachable for {op}: {detail}")]
    Unreachable {
        /// The operation being attempted.
        op: &'static str,
        /// What went wrong reaching it.
        detail: String,
    },
}

/// The control surface hostd drives for one microVM.
///
/// The four operations map onto Firecracker's API (verified against the
/// Firecracker v1.16 docs when the real implementation lands):
/// `boot` → `PUT /actions {InstanceStart}`, `pause` → `PATCH /vm {Paused}`,
/// `resume` → `PATCH /vm {Resumed}`. `shutdown` stops the VM process; it does
/// **not** use `SendCtrlAltDel`, which Firecracker supports on x86 only — a
/// process stop is arch-agnostic and is all v0 needs.
pub trait FirecrackerApi {
    /// Start the configured guest (boot the kernel).
    fn boot(&self) -> impl std::future::Future<Output = Result<(), FcError>> + Send;
    /// Pause the VM (vCPUs stopped); prerequisite for snapshotting.
    fn pause(&self) -> impl std::future::Future<Output = Result<(), FcError>> + Send;
    /// Resume a paused VM.
    fn resume(&self) -> impl std::future::Future<Output = Result<(), FcError>> + Send;
    /// Stop the VM and its Firecracker process.
    fn shutdown(&self) -> impl std::future::Future<Output = Result<(), FcError>> + Send;
}

/// A recording, fault-injecting fake Firecracker for tests.
///
/// It records the ordered sequence of operations it received (so a test can
/// assert hostd issued exactly `boot, pause, resume, shutdown`) and can be
/// primed to fail the next call to a given operation, to exercise error paths.
#[derive(Debug, Default)]
pub struct FakeFc {
    state: Mutex<FakeState>,
}

#[derive(Debug, Default)]
struct FakeState {
    calls: Vec<&'static str>,
    fail_next: Option<(&'static str, FailKind)>,
}

#[derive(Debug, Clone, Copy)]
enum FailKind {
    Rejected,
    Unreachable,
}

impl FakeFc {
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

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        // SAFETY-of-correctness: the only panic source is a poisoned lock, which
        // means a prior test thread panicked while holding it — surfacing that
        // is correct in test code.
        #[allow(clippy::unwrap_used)]
        self.state.lock().unwrap()
    }

    /// Record `op`; honor a primed failure for it.
    fn record(&self, op: &'static str) -> Result<(), FcError> {
        let mut st = self.lock();
        st.calls.push(op);
        if let Some((failed_op, kind)) = st.fail_next
            && failed_op == op
        {
            st.fail_next = None;
            let detail = "injected by FakeFc".to_owned();
            return Err(match kind {
                FailKind::Rejected => FcError::Rejected { op, detail },
                FailKind::Unreachable => FcError::Unreachable { op, detail },
            });
        }
        Ok(())
    }
}

impl FirecrackerApi for FakeFc {
    async fn boot(&self) -> Result<(), FcError> {
        self.record("boot")
    }
    async fn pause(&self) -> Result<(), FcError> {
        self.record("pause")
    }
    async fn resume(&self) -> Result<(), FcError> {
        self.record("resume")
    }
    async fn shutdown(&self) -> Result<(), FcError> {
        self.record("shutdown")
    }
}
