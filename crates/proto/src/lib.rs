//! `proto` — sleepwalk's public contract.
//!
//! Three things live here and nothing else:
//!
//! 1. [`ids`] — newtypes for every domain identifier, so a [`VmId`] can never be
//!    passed where a [`HostId`] is expected.
//! 2. [`vsock`] — the guestd ⇄ hostd wire protocol ([`GuestToHost`] /
//!    [`HostToGuest`]), newline-delimited JSON over vsock.
//! 3. [`fsm`] — the migration state machine, as a compile-time
//!    [typestate][`fsm::Migration`] plus a runtime-inspectable
//!    [`MigrationState`] for logs and metrics.
//!
//! This crate is pure types: no I/O, no async runtime, host-agnostic (tier 1).
//! It mirrors `docs/protocol.md`, the integration contract a non-Rust guest
//! reads (objective O8). The contract is versioned by
//! [`PROTOCOL_VERSION`]; the wire format of every type below is part of it.
#![deny(missing_docs)]
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod fsm;
pub mod ids;
pub mod vsock;
mod wire;

pub use fsm::{Migration, MigrationState};
pub use ids::{GuestdVersion, HostId, Timestamp, TurnId, VmId};
pub use vsock::{GuestToHost, HostToGuest};

/// The guest protocol version this build speaks.
///
/// Bumped whenever the wire shape of [`GuestToHost`] / [`HostToGuest`] changes
/// in a way a guest could observe. Pinned to `v1-draft` until the API freeze in
/// the v0.1.0 release; pre-freeze it may change with a
/// CHANGELOG entry. The boot [`Hello`][GuestToHost::Hello] carries the guest's
/// own [`GuestdVersion`]; hostd checks compatibility against this.
pub const PROTOCOL_VERSION: &str = "v1-draft";
