//! `hostd` — the per-host daemon.
//!
//! Runs Firecracker microVMs on one host: drives their lifecycle, serves their
//! memory pages on restore (UFFD), and moves snapshots between hosts. This first
//! slice is the host-agnostic core:
//!
//! - [`firecracker::FirecrackerApi`] — the control port every Firecracker effect
//!   goes through, with the real [`firecracker::Firecracker`] (HTTP over the
//!   per-VM unix socket) and a recording [`pseudo_firecracker::PseudoFirecracker`]
//!   for tests.
//! - [`vm::Vm`] — the lifecycle orchestrator (boot / pause / resume / shutdown)
//!   that enforces a legal operation order.
//! - [`statedir::VmDir`] — the per-VM on-disk layout and jailer chroot target.
//!
//! Jailer spawn + process teardown and the end-to-end test against a real
//! Firecracker require `/dev/kvm` and land in a later slice; the logic here is
//! tested against [`pseudo_firecracker::PseudoFirecracker`], and the real client
//! against a stub unix-socket server — both with no VM.
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod firecracker;
pub mod pseudo_firecracker;
pub mod statedir;
pub mod vm;

pub use firecracker::{Firecracker, FirecrackerApi, FirecrackerError};
pub use pseudo_firecracker::PseudoFirecracker;
pub use statedir::VmDir;
pub use vm::{LifecycleError, RunState, Vm};
