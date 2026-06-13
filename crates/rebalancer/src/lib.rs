//! `rebalancer` — the control plane.
//!
//! Decides which VM moves where and drives each migration to completion. This
//! first slice is the host-agnostic migration driver:
//!
//! - [`executor::MigrationExecutor`] — the port for migration effects (drain,
//!   snapshot, transfer, restore, cut over, clean up), with a
//!   [`pseudo_executor::PseudoExecutor`] for tests.
//! - [`driver::drive`] — walks proto's migration FSM typestate through the
//!   executor, enforcing the race rule and the point-of-no-return at the type
//!   level.
//!
//! Placement and pressure detection (which VM, which host) and the real
//! executor over the control plane land in later slices.
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod driver;
pub mod executor;
pub mod pseudo_executor;

pub use driver::{AbortReason, MigrationError, MigrationOutcome, drive};
pub use executor::{DrainOutcome, ExecError, MigrationExecutor};
pub use pseudo_executor::PseudoExecutor;
