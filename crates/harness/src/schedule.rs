//! The arrival schedule — when each request is *intended* to fire.
//!
//! Load generation here is **open-loop**: the schedule of intended send times is
//! computed up front from the rate and duration, independent of whether earlier
//! requests have completed. A closed loop (wait-for-response, then send) silently
//! stops sending during a stall and so hides the very latency spike a migration
//! would cause — coordinated omission. Measuring latency from the *intended*
//! send time (see [`crate::recorder`]) is the other half of avoiding it; this
//! module produces those intended times.

use std::time::Duration;

/// The arrival process that spaces requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrivals {
    /// Evenly spaced: one request every `1/rate` seconds.
    Fixed,
    /// Poisson arrivals: exponentially distributed inter-arrival gaps with mean
    /// `1/rate`. Deterministic for a given `seed`, so tests reproduce exactly.
    Poisson {
        /// Seed for the inter-arrival sampler.
        seed: u64,
    },
}

/// A precomputed sequence of intended send times, each an offset from the start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    times: Vec<Duration>,
}

impl Schedule {
    /// Build the schedule for `rate` requests/second over `duration`.
    ///
    /// A non-positive or non-finite `rate` yields an empty schedule. Times are
    /// strictly within `[0, duration)`.
    #[must_use]
    pub fn generate(rate_per_sec: f64, duration: Duration, arrivals: Arrivals) -> Self {
        if !rate_per_sec.is_finite() || rate_per_sec <= 0.0 {
            return Self { times: Vec::new() };
        }
        let horizon = duration.as_secs_f64();
        let times = match arrivals {
            Arrivals::Fixed => Self::fixed(rate_per_sec, horizon),
            Arrivals::Poisson { seed } => Self::poisson(rate_per_sec, horizon, seed),
        };
        Self { times }
    }

    fn fixed(rate: f64, horizon: f64) -> Vec<Duration> {
        let interval = 1.0 / rate;
        let mut times = Vec::new();
        let mut i = 0u64;
        loop {
            let t = i as f64 * interval;
            if t >= horizon {
                break;
            }
            times.push(Duration::from_secs_f64(t));
            i += 1;
        }
        times
    }

    fn poisson(rate: f64, horizon: f64, seed: u64) -> Vec<Duration> {
        let mut rng = SplitMix64::new(seed);
        let mut t = 0.0;
        let mut times = Vec::new();
        loop {
            // Inter-arrival ~ Exp(rate): -ln(1-u)/rate, u in [0,1).
            let u = rng.next_f64();
            t += -(1.0 - u).ln() / rate;
            if t >= horizon {
                break;
            }
            times.push(Duration::from_secs_f64(t));
        }
        times
    }

    /// The intended send times, ascending.
    #[must_use]
    pub fn times(&self) -> &[Duration] {
        &self.times
    }

    /// How many requests the schedule will fire.
    #[must_use]
    pub fn len(&self) -> usize {
        self.times.len()
    }

    /// Whether the schedule fires nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.times.is_empty()
    }
}

/// A tiny seeded PRNG (SplitMix64) — deterministic, dependency-free, enough for
/// reproducible inter-arrival sampling.
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniform `f64` in `[0, 1)`, using the top 53 bits (mantissa width).
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_rate_spaces_requests_evenly_within_the_horizon() {
        let s = Schedule::generate(5.0, Duration::from_secs(1), Arrivals::Fixed);
        // 0.0, 0.2, 0.4, 0.6, 0.8 — five fire before t=1.0.
        assert_eq!(s.len(), 5);
        assert_eq!(s.times()[0], Duration::ZERO);
        assert_eq!(s.times()[1], Duration::from_millis(200));
        assert!(s.times().iter().all(|t| *t < Duration::from_secs(1)));
    }

    #[test]
    fn non_positive_rate_is_empty() {
        assert!(Schedule::generate(0.0, Duration::from_secs(1), Arrivals::Fixed).is_empty());
        assert!(Schedule::generate(-3.0, Duration::from_secs(1), Arrivals::Fixed).is_empty());
        assert!(Schedule::generate(f64::NAN, Duration::from_secs(1), Arrivals::Fixed).is_empty());
    }

    #[test]
    fn poisson_is_deterministic_for_a_seed() {
        let a = Schedule::generate(
            10.0,
            Duration::from_secs(10),
            Arrivals::Poisson { seed: 42 },
        );
        let b = Schedule::generate(
            10.0,
            Duration::from_secs(10),
            Arrivals::Poisson { seed: 42 },
        );
        assert_eq!(a, b);
        // A different seed gives a different sequence.
        let c = Schedule::generate(10.0, Duration::from_secs(10), Arrivals::Poisson { seed: 7 });
        assert_ne!(a, c);
    }

    #[test]
    fn poisson_times_are_monotonic_and_roughly_match_the_rate() {
        let s = Schedule::generate(
            10.0,
            Duration::from_secs(100),
            Arrivals::Poisson { seed: 1 },
        );
        // ~rate * duration arrivals expected (1000); allow a wide band.
        assert!(s.len() > 800 && s.len() < 1200, "got {}", s.len());
        assert!(s.times().windows(2).all(|w| w[0] <= w[1]), "not monotonic");
        assert!(s.times().iter().all(|t| *t < Duration::from_secs(100)));
    }
}
