//! The open-loop load generator.
//!
//! Given an arrival [`Schedule`] and a [`TurnDriver`], fires each turn at its
//! **intended** time — independently of whether earlier turns have finished — and
//! records latency from that intended time. This is the whole point: when the
//! workload stalls (a migration freezes the guest), turns scheduled during the
//! stall still "fire" on time and their latency reflects the delay. A closed loop
//! (wait for the response, then send) would stop sending during the stall and
//! hide exactly the spike a migration causes — coordinated omission. Measuring
//! from the intended time, not the actual send, is the other half of avoiding it.
//!
//! The driver is the seam to the workload: today a test fake, later a real client
//! issuing turns to a guest over vsock. Swapping it does not change the
//! measurement, so the migration-impact number is produced the same way against a
//! real VM as against the fake.

use std::future::Future;
use std::sync::{Arc, Mutex};

use tokio::time::Instant;

use crate::recorder::{LatencyRecorder, LatencyStats};
use crate::schedule::Schedule;

/// Issues one unit of guest work (a "turn") and returns when it completes.
pub trait TurnDriver: Send + Sync + 'static {
    /// Run turn `turn`, resolving when the guest reports it done.
    fn run_turn(&self, turn: u64) -> impl Future<Output = ()> + Send;
}

/// Drive `schedule` against `driver`, open-loop, and return the latency
/// distribution measured from each turn's intended time.
///
/// Each turn is its own task that waits until its intended instant, runs, and
/// records `completed - intended` — so a slow turn never delays another's start.
pub async fn run_load<D: TurnDriver>(schedule: &Schedule, driver: Arc<D>) -> LatencyStats {
    let recorder = Arc::new(Mutex::new(LatencyRecorder::new()));
    let start = Instant::now();

    let mut tasks = Vec::with_capacity(schedule.len());
    for (i, &offset) in schedule.times().iter().enumerate() {
        let driver = Arc::clone(&driver);
        let recorder = Arc::clone(&recorder);
        tasks.push(tokio::spawn(async move {
            tokio::time::sleep_until(start + offset).await;
            driver.run_turn(i as u64).await;
            let completed = Instant::now().duration_since(start);
            // Lock is held only for the record; poisoning means a prior turn
            // panicked, which a load run should surface.
            #[allow(clippy::unwrap_used)]
            let _ = recorder.lock().unwrap().record(offset, completed);
        }));
    }
    for task in tasks {
        let _ = task.await;
    }

    #[allow(clippy::unwrap_used)]
    let stats = recorder.lock().unwrap().summary();
    stats
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::schedule::Arrivals;

    /// A driver whose every turn takes a fixed time — stands in for a workload
    /// (and, with a large duration, for the freeze a migration would impose).
    struct FixedLatency(Duration);
    impl TurnDriver for FixedLatency {
        async fn run_turn(&self, _turn: u64) {
            tokio::time::sleep(self.0).await;
        }
    }

    /// With paused time, every turn's measured latency is exactly the driver's
    /// per-turn cost, regardless of how the arrivals overlap.
    #[tokio::test(start_paused = true)]
    async fn measures_intended_time_latency() {
        let schedule = Schedule::generate(5.0, Duration::from_secs(2), Arrivals::Fixed);
        assert!(schedule.len() >= 5);
        let stats = run_load(
            &schedule,
            Arc::new(FixedLatency(Duration::from_millis(200))),
        )
        .await;

        assert_eq!(stats.count, schedule.len() as u64);
        // Each turn fires at its intended instant, then takes 200ms — so latency
        // measured from intended is ~200ms across the board (a hair of scheduling
        // jitter + histogram precision, not coordinated omission).
        assert!(
            stats.p50 >= Duration::from_millis(200) && stats.p50 < Duration::from_millis(205),
            "p50 {:?}",
            stats.p50
        );
        assert!(
            stats.p99 >= Duration::from_millis(200) && stats.p99 < Duration::from_millis(210),
            "p99 {:?}",
            stats.p99
        );
    }

    /// A turn that stalls far longer than the arrival gap (a freeze) shows its
    /// full latency from the intended time — coordinated omission is avoided.
    #[tokio::test(start_paused = true)]
    async fn a_long_stall_is_not_hidden() {
        // One turn at t=0; the driver stalls 1s (a "freeze"). Intended-time
        // latency captures the whole second.
        let schedule = Schedule::generate(1.0, Duration::from_millis(500), Arrivals::Fixed);
        let stats = run_load(&schedule, Arc::new(FixedLatency(Duration::from_secs(1)))).await;
        assert_eq!(stats.count, 1);
        assert!(
            stats.max >= Duration::from_secs(1),
            "stall latency: {:?}",
            stats.max
        );
    }

    /// An empty schedule drives nothing.
    #[tokio::test(start_paused = true)]
    async fn empty_schedule_records_nothing() {
        let schedule = Schedule::generate(0.0, Duration::from_secs(1), Arrivals::Fixed);
        let stats = run_load(&schedule, Arc::new(FixedLatency(Duration::ZERO))).await;
        assert_eq!(stats.count, 0);
    }
}
