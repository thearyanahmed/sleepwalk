//! `guestd` — the in-VM supervisor.
//!
//! Runs inside each microVM as hostd's representative: it announces the VM at
//! boot, takes secrets over vsock (never the rootfs or cmdline), reports turn
//! boundaries so hostd can verify quiescence, and holds the drain gate that
//! makes "migrate only at a safe point" actually safe. This first slice is the
//! host-agnostic core:
//!
//! - [`channel::GuestChannel`] — the vsock seam, plus a scripted
//!   [`channel::FakeChannel`] for tests.
//! - [`guest::Guest`] — the supervisor state machine (handshake, turn signals,
//!   drain gate).
//!
//! The real `AF_VSOCK` transport and process-wrapping require a running guest
//! and land in a later slice; everything here is tested against the fake.
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod channel;
pub mod guest;

pub use channel::{ChannelError, FakeChannel, GuestChannel};
pub use guest::{Guest, GuestError, StartOutcome};
