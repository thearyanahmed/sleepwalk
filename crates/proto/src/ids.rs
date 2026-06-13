//! Domain identifiers and the wire timestamp, each a distinct newtype.
//!
//! The point is type separation, not convenience: the compiler rejects passing a
//! [`VmId`] where a [`HostId`] belongs, and there are no sentinel values (no
//! empty-string host, no `0` turn standing in for "none" — absence is
//! `Option`).

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Identifies one microVM across its whole life, stable across migration.
///
/// A UUID rather than a host-scoped counter precisely *because* the VM moves
/// between hosts: the identity must not be tied to a host or a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VmId(Uuid);

impl VmId {
    /// Mint a fresh random VM id.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wrap an existing UUID (e.g. parsed from a state dir name).
    #[must_use]
    pub const fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    /// The underlying UUID.
    #[must_use]
    pub const fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl Default for VmId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for VmId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Names a host in the fleet (e.g. `host-a`, a hostname, a chroot label).
///
/// Opaque on purpose: hostd assigns the strings, proto only carries them.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostId(String);

impl HostId {
    /// Construct from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the underlying string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HostId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Monotonic per-VM turn counter. A "turn" is one unit of guest work (an agent
/// turn, a synthetic burst); the race rule is defined over these. Absence of an
/// in-flight turn is `Option<TurnId>::None`, never turn 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnId(u64);

impl TurnId {
    /// The first turn id. Subsequent turns come from [`TurnId::next`].
    pub const FIRST: Self = Self(0);

    /// Wrap a raw counter value.
    #[must_use]
    pub const fn from_u64(n: u64) -> Self {
        Self(n)
    }

    /// The raw counter value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The next turn id. Saturates at [`u64::MAX`] rather than wrapping; a fleet
    /// that issues 2^64 turns has bigger problems than a stuck counter.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

/// The guestd build version from the boot [`Hello`][crate::GuestToHost::Hello].
///
/// A free-form version string (semver in practice) that hostd checks against
/// [`PROTOCOL_VERSION`][crate::PROTOCOL_VERSION] for compatibility.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentVersion(String);

impl AgentVersion {
    /// Construct from any string-like value.
    pub fn new(v: impl Into<String>) -> Self {
        Self(v.into())
    }

    /// Borrow the underlying string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A wall-clock instant as the *guest* observed it: nanoseconds since the Unix
/// epoch, carried on the wire as a single JSON integer.
///
/// Guest-sourced and therefore suspect across a migration: the guest clock
/// freezes at snapshot time and only resyncs on
/// [`Resumed`][crate::GuestToHost::Resumed] (clock fix-up).
/// Two timestamps that straddle a migration are **not** comparable until that
/// resync — treat this as an event tag, not a monotonic clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(u64);

impl Timestamp {
    /// Build from nanoseconds since the Unix epoch.
    #[must_use]
    pub const fn from_nanos(nanos_since_epoch: u64) -> Self {
        Self(nanos_since_epoch)
    }

    /// Nanoseconds since the Unix epoch.
    #[must_use]
    pub const fn as_nanos(self) -> u64 {
        self.0
    }

    /// As a [`Duration`] since the Unix epoch, for arithmetic against other
    /// epoch-relative instants.
    #[must_use]
    pub const fn since_epoch(self) -> Duration {
        Duration::from_nanos(self.0)
    }
}
