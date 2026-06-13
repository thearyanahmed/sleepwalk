//! Serde glue for the wire format.
//!
//! Rust types use [`Duration`] (per the coding approach: time is never a bare
//! integer of ambiguous unit). The wire, read by non-Rust guests, wants a plain
//! integer — so durations cross the boundary as whole milliseconds.

use std::time::Duration;

/// (De)serialize a [`Duration`] as a `u64` count of whole milliseconds.
///
/// Use via `#[serde(with = "crate::wire::millis")]` on a `Duration` field whose
/// wire name ends in `_ms`. Sub-millisecond precision is truncated on the way
/// out — fine for the deadlines this protocol carries, which are coarse by
/// nature.
pub mod millis {
    use super::Duration;
    use serde::{Deserialize, Deserializer, Serializer};

    /// Serialize as whole milliseconds.
    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_millis().min(u128::from(u64::MAX)) as u64)
    }

    /// Deserialize from whole milliseconds.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let ms = u64::deserialize(d)?;
        Ok(Duration::from_millis(ms))
    }
}
