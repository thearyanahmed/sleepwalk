//! `hostd` — the per-host daemon.
//!
//! Runs Firecracker microVMs on one host: drives their lifecycle, serves their
//! memory pages on restore (UFFD), and moves snapshots between hosts. This first
//! slice is the host-agnostic core:
//!
//! - [`fc::FirecrackerApi`] — the control port every Firecracker effect goes
//!   through, plus a recording [`fc::FakeFc`] for tests.
//! - [`vm::Vm`] — the lifecycle orchestrator (boot / pause / resume / shutdown)
//!   that enforces a legal operation order.
//! - [`statedir::VmDir`] — the per-VM on-disk layout and jailer chroot target.
//!
//! The real Firecracker implementation (HTTP over the per-VM unix socket) and
//! jailer spawn require `/dev/kvm` and land in a later slice; everything here is
//! tested against the fake, with no VM.
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod fc;
pub mod statedir;
pub mod vm;

pub use fc::real::RealFc;
pub use fc::{FakeFc, FcError, FirecrackerApi};
pub use statedir::VmDir;
pub use vm::{LifecycleError, RunState, Vm};
