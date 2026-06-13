//! Latency recording with intended-send-time accounting.
//!
//! The other half of avoiding coordinated omission (see [`crate::schedule`]):
//! a request's latency is measured from when it was *intended* to fire, not when
//! it actually went out. If sends stall, the stall shows up in the latency — it
//! is not hidden. Latencies feed an [`HdrHistogram`](hdrhistogram::Histogram)
//! for accurate tail percentiles.

use std::time::Duration;

use hdrhistogram::Histogram;
use thiserror::Error;

/// A failure recording one latency sample.
#[derive(Debug, Error)]
pub enum RecordError {
    /// The completion time was before the intended send time — impossible for a
    /// real request, so a clock or wiring bug.
    #[error("completion {completed:?} precedes intended send {intended:?}")]
    CompletedBeforeIntended {
        /// The intended send time.
        intended: Duration,
        /// The (earlier) completion time.
        completed: Duration,
    },

    /// The latency was outside the histogram's representable range.
    #[error("latency {0:?} out of histogram range")]
    OutOfRange(Duration),
}

/// Accumulates request latencies and reports their distribution.
#[derive(Debug)]
pub struct LatencyRecorder {
    hist: Histogram<u64>,
}

impl LatencyRecorder {
    /// A recorder keeping three significant figures (auto-resizing range).
    #[must_use]
    pub fn new() -> Self {
        // `new(3)` is auto-resizing, so large latencies never error on range.
        #[allow(clippy::expect_used)]
        let hist = Histogram::<u64>::new(3).expect("3 is a valid sigfig count");
        Self { hist }
    }

    /// Record a request from its intended send time and completion time. Latency
    /// is `completed - intended` — the coordinated-omission-safe measurement.
    pub fn record(&mut self, intended: Duration, completed: Duration) -> Result<(), RecordError> {
        let latency =
            completed
                .checked_sub(intended)
                .ok_or(RecordError::CompletedBeforeIntended {
                    intended,
                    completed,
                })?;
        self.record_latency(latency)
    }

    /// Record a latency directly (when it is already computed).
    pub fn record_latency(&mut self, latency: Duration) -> Result<(), RecordError> {
        let micros = u64::try_from(latency.as_micros()).unwrap_or(u64::MAX);
        self.hist
            .record(micros)
            .map_err(|_| RecordError::OutOfRange(latency))
    }

    /// The number of samples recorded.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.hist.len()
    }

    /// A snapshot of the latency distribution.
    #[must_use]
    pub fn summary(&self) -> LatencyStats {
        let q = |quant: f64| Duration::from_micros(self.hist.value_at_quantile(quant));
        LatencyStats {
            count: self.hist.len(),
            p50: q(0.50),
            p90: q(0.90),
            p99: q(0.99),
            p99_9: q(0.999),
            max: Duration::from_micros(self.hist.max()),
        }
    }
}

impl Default for LatencyRecorder {
    fn default() -> Self {
        Self::new()
    }
}

/// A summary of recorded latencies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatencyStats {
    /// Number of samples.
    pub count: u64,
    /// Median.
    pub p50: Duration,
    /// 90th percentile.
    pub p90: Duration,
    /// 99th percentile.
    pub p99: Duration,
    /// 99.9th percentile.
    pub p99_9: Duration,
    /// Maximum observed.
    pub max: Duration,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_track_the_recorded_distribution() {
        let mut r = LatencyRecorder::new();
        for ms in 1..=100u64 {
            r.record_latency(Duration::from_millis(ms)).expect("record");
        }
        let s = r.summary();
        assert_eq!(s.count, 100);
        // ~50ms median, ~100ms max, within histogram precision.
        assert!(s.p50 >= Duration::from_millis(49) && s.p50 <= Duration::from_millis(51));
        assert!(s.max >= Duration::from_millis(99));
        assert!(s.p99 >= s.p90 && s.p90 >= s.p50);
    }

    #[test]
    fn latency_is_measured_from_intended_not_actual_send() {
        let mut r = LatencyRecorder::new();
        // Two requests completing at the same instant (200ms), but intended at
        // different times. The one intended earlier has the larger latency —
        // the stall is not hidden.
        r.record(Duration::ZERO, Duration::from_millis(200))
            .expect("record a");
        r.record(Duration::from_millis(100), Duration::from_millis(200))
            .expect("record b");
        let s = r.summary();
        assert_eq!(s.count, 2);
        assert!(s.max >= Duration::from_millis(199)); // the 200ms-from-intended one
    }

    #[test]
    fn completion_before_intended_is_rejected() {
        let mut r = LatencyRecorder::new();
        let err = r
            .record(Duration::from_millis(100), Duration::from_millis(50))
            .expect_err("must reject");
        assert!(matches!(err, RecordError::CompletedBeforeIntended { .. }));
        assert_eq!(r.count(), 0);
    }
}
