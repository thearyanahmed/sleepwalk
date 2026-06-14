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
//! - [`placement::pick_victim`] — the pressure-relief heuristic: the most-idle
//!   VM on the hottest host, moved to the coolest host that can take it.
//!
//! The real executor over the control plane lands in a later slice.
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod driver;
pub mod executor;
pub mod placement;
pub mod pseudo_executor;

pub use driver::{AbortReason, MigrationError, MigrationOutcome, drive};
pub use executor::{DrainOutcome, ExecError, MigrationExecutor};
pub use placement::{Placement, Pressure, Rebalance, pick_victim};
pub use pseudo_executor::PseudoExecutor;
