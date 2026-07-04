//! Exponential backoff with optional full-jitter, used by the reconnect loop.
//!
//! Deterministic when `jitter` is disabled (unit-tested progression); the
//! jitter path uses a small in-struct LCG so it needs no `rand` dependency and
//! stays within documented bounds for property checks.

use std::time::Duration;

/// Classic capped exponential backoff.
#[derive(Debug, Clone)]
pub struct ExponentialBackoff {
    initial: Duration,
    max: Duration,
    multiplier: f64,
    jitter: bool,
    current: Duration,
    seed: u64,
}

impl Default for ExponentialBackoff {
    fn default() -> Self {
        Self::new(
            Duration::from_millis(500),
            Duration::from_secs(30),
            2.0,
            true,
        )
    }
}

impl ExponentialBackoff {
    /// Build a backoff schedule. `multiplier` should be >= 1.0.
    pub fn new(initial: Duration, max: Duration, multiplier: f64, jitter: bool) -> Self {
        Self {
            initial,
            max,
            multiplier: multiplier.max(1.0),
            jitter,
            current: initial,
            seed: 0x9E3779B97F4A7C15,
        }
    }

    /// Reset the schedule back to the initial delay (call after a successful
    /// connect so the next disconnect starts small again).
    pub fn reset(&mut self) {
        self.current = self.initial;
    }

    /// The delay to wait before the next attempt, then advance the schedule.
    /// The first call returns ~`initial`; subsequent calls grow geometrically
    /// up to `max`. With jitter enabled the returned value is in
    /// `[base/2, base]` (full jitter, lower-half) to spread reconnect storms.
    pub fn next_backoff(&mut self) -> Duration {
        let base = self.current;
        // Advance for next time (saturating at max).
        let grown = base.mul_f64(self.multiplier);
        self.current = if grown > self.max { self.max } else { grown };

        if self.jitter {
            let r = self.rand01();
            base.mul_f64(0.5 + 0.5 * r)
        } else {
            base
        }
    }

    /// Deterministic LCG in [0,1). Only used for jitter.
    fn rand01(&mut self) -> f64 {
        // SplitMix64-ish step.
        self.seed = self
            .seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let x = (self.seed >> 11) as f64;
        x / (1u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_progression_without_jitter() {
        let mut b = ExponentialBackoff::new(
            Duration::from_millis(100),
            Duration::from_millis(1000),
            2.0,
            false,
        );
        assert_eq!(b.next_backoff(), Duration::from_millis(100));
        assert_eq!(b.next_backoff(), Duration::from_millis(200));
        assert_eq!(b.next_backoff(), Duration::from_millis(400));
        assert_eq!(b.next_backoff(), Duration::from_millis(800));
        // capped at max
        assert_eq!(b.next_backoff(), Duration::from_millis(1000));
        assert_eq!(b.next_backoff(), Duration::from_millis(1000));
    }

    #[test]
    fn reset_returns_to_initial() {
        let mut b = ExponentialBackoff::new(
            Duration::from_millis(100),
            Duration::from_secs(10),
            3.0,
            false,
        );
        b.next_backoff();
        b.next_backoff();
        b.reset();
        assert_eq!(b.next_backoff(), Duration::from_millis(100));
    }

    #[test]
    fn jitter_stays_within_lower_half_bounds() {
        let mut b = ExponentialBackoff::new(
            Duration::from_millis(1000),
            Duration::from_secs(60),
            2.0,
            true,
        );
        for _ in 0..200 {
            let base = b.current;
            let d = b.next_backoff();
            assert!(d <= base, "jitter must not exceed base");
            assert!(d >= base.mul_f64(0.5), "jitter must stay >= base/2");
        }
    }
}
