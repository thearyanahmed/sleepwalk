//! `harness` — load generator, latency recorder, and chaos harness.
//!
//! This first slice is the measurement math, the part that has to be right for
//! any number to be trustworthy:
//!
//! - [`schedule::Schedule`] — the open-loop arrival schedule (fixed or Poisson),
//!   computed up front so a stall cannot silently stop sending.
//! - [`recorder::LatencyRecorder`] — latency measured from the *intended* send
//!   time and aggregated into an HdrHistogram for accurate tail percentiles.
//!
//! Together they avoid coordinated omission, the failure mode that would hide
//! exactly the latency spike a migration could cause. The request transport
//! (driving turns through hostd) lands in a later slice.
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod recorder;
pub mod schedule;

pub use recorder::{LatencyRecorder, LatencyStats, RecordError};
pub use schedule::{Arrivals, Schedule};
