//! `guestd` — the in-VM supervisor.
//!
//! Runs inside each microVM as hostd's representative: it announces the VM at
//! boot, takes secrets over vsock (never the rootfs or cmdline), reports turn
//! boundaries so hostd can verify quiescence, and holds the drain gate that
//! makes "migrate only at a safe point" actually safe. This first slice is the
//! host-agnostic core:
//!
//! - [`channel::GuestChannel`] — the vsock seam, with a scripted
//!   [`pseudo_channel::PseudoChannel`] stand-in for tests.
//! - [`guest::Guest`] — the supervisor state machine (handshake, turn signals,
//!   drain gate).
//! - [`clock::ClockFixup`] — the post-restore clock correction: maps the guest's
//!   frozen clock back onto true wall-clock time so timestamps stay comparable
//!   across a migration.
//!
//! The real transport, `VsockChannel` (`AF_VSOCK`), and process-wrapping require
//! a running guest and land in a later slice; everything here is tested against
//! the fake.
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod channel;
pub mod clock;
pub mod framing;
pub mod guest;
pub mod pseudo_channel;

pub use channel::{ChannelError, GuestChannel};
pub use clock::ClockFixup;
pub use framing::JsonLineChannel;
pub use guest::{Guest, GuestError, StartOutcome};
pub use pseudo_channel::PseudoChannel;
