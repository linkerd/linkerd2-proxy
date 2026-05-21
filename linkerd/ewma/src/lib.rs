#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use tokio::time;

/// An exponentially-weighted moving average.
#[derive(Debug)]
pub struct Ewma {
    value: f64,
    decay: f64,
    timestamp: time::Instant,
}

// === impl Ewma ===

impl Ewma {
    #[must_use]
    pub fn new(decay: time::Duration, timestamp: time::Instant) -> Self {
        Self {
            decay: decay.as_secs_f64(),
            timestamp,
            value: f64::INFINITY,
        }
    }

    /// Returns the current value of the average.
    pub fn get(&self) -> f64 {
        self.value
    }

    /// Updates the weighted moving average with a new value and timestamp.
    ///
    /// Precondition: `value` must not be NaN. Passing NaN poisons the EWMA
    /// irreversibly (all subsequent reads return NaN).
    pub fn add(&mut self, value: f64, ts: time::Instant) {
        debug_assert!(!value.is_nan(), "EWMA input value must not be NaN");
        if ts <= self.timestamp {
            return;
        }
        if self.value == f64::INFINITY {
            self.value = value;
            self.timestamp = ts;
            return;
        }

        self.value = {
            let elapsed = ts.saturating_duration_since(self.timestamp);
            let alpha = 1.0 - (-elapsed.as_secs_f64() / self.decay).exp();
            self.value * (1.0 - alpha) + value * alpha
        };

        self.timestamp = ts;
    }

    pub fn add_peak(&mut self, value: f64, ts: time::Instant) {
        if self.value < value {
            self.value = value;
            self.timestamp = ts;
            return;
        }
        self.add(value, ts)
    }

    /// Computes 1/elapsed since the last update and feeds it through `add()`.
    pub fn add_rate(&mut self, ts: time::Instant) {
        if ts <= self.timestamp {
            return;
        }
        let elapsed = ts.saturating_duration_since(self.timestamp);
        if !elapsed.is_zero() {
            self.add(1.0 / elapsed.as_secs_f64(), ts);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{Duration, Instant};

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_new() {
        let now = Instant::now();
        let ewma = Ewma::new(Duration::from_secs(10), now);
        assert_eq!(ewma.get(), f64::INFINITY);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_add() {
        let now = Instant::now();
        let mut ewma = Ewma::new(Duration::from_secs(10), now);
        ewma.add(1.0, now + Duration::from_secs(1));
        assert_eq!(ewma.get(), 1.0);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_add_rate() {
        let now = Instant::now();
        let mut ewma = Ewma::new(Duration::from_secs(10), now);
        ewma.add_rate(now + Duration::from_secs(1));
        assert_eq!(ewma.get(), 1.0);
        ewma.add_rate(now + Duration::from_secs(3));
        assert_eq!(ewma.get(), 0.9093653765389909);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_add_peak() {
        let now = Instant::now();
        let mut ewma = Ewma::new(Duration::from_secs(10), now);
        ewma.add_peak(1.0, now + Duration::from_secs(1));
        assert_eq!(ewma.get(), 1.0);
        ewma.add_peak(2.0, now + Duration::from_secs_f64(1.5));
        assert_eq!(ewma.get(), 2.0);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_decay() {
        let now = Instant::now();
        let mut ewma = Ewma::new(Duration::from_secs(10), now);
        ewma.add(1.0, now + Duration::from_secs(1));
        assert_eq!(ewma.get(), 1.0);
        ewma.add(0.0, now + Duration::from_secs(11));
        assert_eq!(ewma.get(), 0.36787944117144233);
    }
}
