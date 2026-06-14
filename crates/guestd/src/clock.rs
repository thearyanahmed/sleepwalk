//! Clock fix-up across a migration.
//!
//! A microVM's wall clock **freezes at snapshot time** and resumes from that
//! frozen value on the target host — so right after a restore the guest believes
//! no time passed, while the real world advanced by the whole migration. Left
//! uncorrected this breaks anything time-sensitive (TLS validity, token expiry)
//! and makes two timestamps that straddle a migration incomparable (see
//! [`Timestamp`]).
//!
//! [`ClockFixup`] is the pure half of the fix: given the guest's own clock
//! reading at resume and an authoritative wall-clock reading for the same
//! instant (supplied by the host, which never froze), it computes the signed
//! correction and [`correct`](ClockFixup::correct)s any later guest reading back
//! onto the true timeline. Applying the correction to the live system clock
//! (`clock_settime`) is the guest-OS side and lands with the real resume path;
//! the arithmetic here is what the measurement harness uses to line up
//! turn latencies recorded on either side of a move.

use proto::Timestamp;

/// A signed clock correction: the offset (in nanoseconds) to add to a frozen
/// guest reading to recover true wall-clock time after a restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockFixup {
    /// `authoritative - guest_at_resume`, in nanoseconds. Positive in the normal
    /// case (the world moved forward while the guest was frozen).
    offset_nanos: i128,
}

impl ClockFixup {
    /// Compute the correction from the guest's clock reading at resume and the
    /// authoritative wall-clock reading for that same instant.
    #[must_use]
    pub fn between(guest_at_resume: Timestamp, authoritative: Timestamp) -> Self {
        let offset_nanos =
            i128::from(authoritative.as_nanos()) - i128::from(guest_at_resume.as_nanos());
        Self { offset_nanos }
    }

    /// The identity correction — no skew (e.g. for code paths with no migration).
    #[must_use]
    pub const fn none() -> Self {
        Self { offset_nanos: 0 }
    }

    /// The signed offset in nanoseconds (`authoritative - guest`).
    #[must_use]
    pub const fn offset_nanos(&self) -> i128 {
        self.offset_nanos
    }

    /// Whether the clock needs adjusting at all.
    #[must_use]
    pub const fn is_skewed(&self) -> bool {
        self.offset_nanos != 0
    }

    /// Map a post-resume guest reading back onto the true timeline. Saturates at
    /// the `u64` bounds rather than wrapping — a correction that would push a
    /// timestamp below the epoch or past `u64::MAX` is clamped, not silently
    /// wrapped into a nonsense instant.
    #[must_use]
    pub fn correct(&self, observed: Timestamp) -> Timestamp {
        let corrected = i128::from(observed.as_nanos()) + self.offset_nanos;
        let clamped = corrected.clamp(0, i128::from(u64::MAX));
        // The clamp guarantees the value fits in u64.
        Timestamp::from_nanos(clamped as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(n: u64) -> Timestamp {
        Timestamp::from_nanos(n)
    }

    /// The normal case: the guest resumes believing it is still snapshot-time,
    /// the host knows the true time, and the forward offset corrects later guest
    /// readings onto the real timeline.
    #[test]
    fn forward_skew_is_corrected() {
        // Snapshot froze the guest clock at 1_000; the migration took 500 of
        // real time, so the authoritative clock reads 1_500 at resume.
        let fixup = ClockFixup::between(ts(1_000), ts(1_500));
        assert!(fixup.is_skewed());
        assert_eq!(fixup.offset_nanos(), 500);

        // A turn the guest later stamps at its (still-behind) clock value 1_200
        // maps to true time 1_700.
        assert_eq!(fixup.correct(ts(1_200)), ts(1_700));
    }

    /// With no skew the correction is the identity.
    #[test]
    fn no_skew_is_identity() {
        let fixup = ClockFixup::between(ts(2_000), ts(2_000));
        assert!(!fixup.is_skewed());
        assert_eq!(fixup.correct(ts(9_999)), ts(9_999));
        assert_eq!(ClockFixup::none(), fixup);
    }

    /// After correction, a timestamp from before the migration and one from
    /// after are comparable on the same timeline — the property the raw
    /// `Timestamp` cannot offer across a move.
    #[test]
    fn corrected_timestamps_are_comparable_across_the_move() {
        // Pre-migration true time of an event.
        let before = ts(1_400);
        // Guest froze at 1_000, resumed when true time was 1_500.
        let fixup = ClockFixup::between(ts(1_000), ts(1_500));
        // The guest stamps a post-resume event at its clock value 1_100, which
        // corrects to true time 1_600 — correctly *after* `before`.
        let after = fixup.correct(ts(1_100));
        assert!(
            after > before,
            "corrected post-move ts must sort after pre-move"
        );
    }

    /// A negative correction that would underflow the epoch is clamped to zero,
    /// never wrapped.
    #[test]
    fn backward_correction_saturates_at_epoch() {
        let fixup = ClockFixup::between(ts(1_000), ts(100)); // offset -900
        assert_eq!(fixup.correct(ts(500)), ts(0));
    }
}
