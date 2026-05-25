//! Exponentially weighted moving average (EWMA) with time-based decay.
//!
//! A standalone EWMA implementation that supports non-mutating time-projected
//! reads via [`Ewma::get_at`] and dual-metric tracking (RTT + penalty) under a
//! single lock. Tower's internal `RttEstimate` is private, mutates on read, and
//! cannot support the penalty dimension that failure-aware load balancing needs.

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use tokio::time;

/// Minimum decay duration to prevent division-by-zero in EWMA computations.
/// Chosen as the smallest Duration that is strictly positive without overriding
/// validated configs from the control plane (CP should reject decay=0).
pub const MIN_DECAY: time::Duration = time::Duration::from_millis(1);

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
            decay: decay.max(MIN_DECAY).as_secs_f64(),
            timestamp,
            value: f64::INFINITY,
        }
    }

    /// Creates an EWMA with a specific initial value.
    ///
    /// This constructor allows setting an initial value, useful for
    /// success rate tracking where you want to start at 100% (1.0)
    /// success rate.
    #[must_use]
    pub fn new_with_value(decay: time::Duration, timestamp: time::Instant, initial: f64) -> Self {
        debug_assert!(!initial.is_nan(), "EWMA initial value must not be NaN");
        Self {
            decay: decay.max(MIN_DECAY).as_secs_f64(),
            timestamp,
            value: initial,
        }
    }

    /// Resets the EWMA to a new value and timestamp.
    ///
    /// This overwrites the current value and timestamp, useful for
    /// resetting success rate tracking after recovery from a tripped state.
    ///
    /// Precondition: `ts` should be <= the timestamps passed to subsequent
    /// `add()` calls. If it is not, those `add()` calls are silently
    /// dropped because `add()` discards samples whose timestamp is at or
    /// before the stored one.
    pub fn reset(&mut self, value: f64, ts: time::Instant) {
        debug_assert!(!value.is_nan(), "EWMA reset value must not be NaN");
        self.value = value;
        self.timestamp = ts;
    }

    /// Returns the current value of the average.
    pub fn get(&self) -> f64 {
        self.value
    }

    /// Returns the decayed value projected to the given time, without modifying stored state.
    ///
    /// Instead of returning the raw stored value, this applies exponential decay based
    /// on elapsed time since the last update. This is required for load balancing where
    /// stale measurements should lose influence over time.
    pub fn get_at(&self, now: time::Instant) -> f64 {
        debug_assert!(!self.value.is_nan(), "EWMA value must not be NaN");

        if self.value.is_infinite() || now <= self.timestamp {
            return self.value;
        }
        let elapsed = now.saturating_duration_since(self.timestamp);
        self.value * (-elapsed.as_secs_f64() / self.decay).exp()
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

    /// Updates the EWMA with a peak value, replacing the current value if the
    /// new value exceeds the decayed projection.
    ///
    /// When replacement occurs, the stored timestamp is set to `ts`, which
    /// may be earlier than the previously stored timestamp. This resets the
    /// decay reference point, so subsequent projections via `get_at()` measure
    /// elapsed time from `ts`.
    pub fn add_peak(&mut self, value: f64, ts: time::Instant) {
        debug_assert!(!value.is_nan(), "EWMA peak value must not be NaN");
        if self.value.is_infinite() || self.get_at(ts) < value {
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

    // Literal value for exp(-1.0), since it's not const
    const EXP_NEG1: f64 = 0.36787944117144233;

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
        assert_eq!(ewma.get(), EXP_NEG1);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_new_with_value() {
        let now = Instant::now();
        let ewma = Ewma::new_with_value(Duration::from_secs(10), now, 1.0);

        assert_eq!(ewma.get(), 1.0);

        // Verify this behaves like a normal EWMA after initialization
        let mut ewma = Ewma::new_with_value(Duration::from_secs(10), now, 1.0);

        ewma.add(0.0, now + Duration::from_secs(10));
        // After one decay period value decays towards zero.
        assert_eq!(ewma.get(), EXP_NEG1);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_add_peak_from_infinity_same_timestamp() {
        let now = Instant::now();
        let mut ewma = Ewma::new(Duration::from_secs(10), now);

        assert_eq!(ewma.get(), f64::INFINITY);

        // Same timestamp as construction. The first real value should always
        // take effect regardless of timestamp.
        ewma.add_peak(0.5, now);
        assert_eq!(ewma.get(), 0.5);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_add_peak_replaces_decayed_value() {
        let now = Instant::now();
        let mut ewma = Ewma::new(Duration::from_secs(10), now);

        // Set initial peak of 10.0 at t=1s
        ewma.add_peak(10.0, now + Duration::from_secs(1));
        assert_eq!(ewma.get(), 10.0);

        // After 25s of decay (t=26s), the decayed projection is:
        // 10.0 * exp(-25/10) = 10.0 * exp(-2.5) = 0.8208...
        // Since 0.8208 < 1.0, the new value should replace the stale peak.
        ewma.add_peak(1.0, now + Duration::from_secs(26));
        assert_eq!(ewma.get(), 1.0);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_zero_decay_clamped_in_new() {
        let now = Instant::now();
        let zero = Ewma::new(Duration::ZERO, now);
        let min = Ewma::new(Duration::from_millis(1), now);

        // Both should produce identical behavior since ZERO gets clamped to 1ms
        assert_eq!(zero.get(), min.get());

        // After adding a value, get_at should produce finite, identical results
        let mut zero = Ewma::new(Duration::ZERO, now);
        let mut min = Ewma::new(Duration::from_millis(1), now);
        zero.add(5.0, now + Duration::from_secs(1));
        min.add(5.0, now + Duration::from_secs(1));

        let projected_zero = zero.get_at(now + Duration::from_secs(2));
        let projected_min = min.get_at(now + Duration::from_secs(2));

        assert!(
            projected_zero.is_finite(),
            "get_at must be finite with clamped decay"
        );
        assert!(
            !projected_zero.is_nan(),
            "get_at must not be NaN with clamped decay"
        );
        assert_eq!(projected_zero, projected_min);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_zero_decay_clamped_in_new_with_value() {
        let now = Instant::now();
        let zero = Ewma::new_with_value(Duration::ZERO, now, 1.0);
        let min = Ewma::new_with_value(Duration::from_millis(1), now, 1.0);

        let projected_zero = zero.get_at(now + Duration::from_secs(1));
        let projected_min = min.get_at(now + Duration::from_secs(1));
        assert!(
            projected_zero.is_finite(),
            "get_at must be finite with clamped decay"
        );
        assert!(
            !projected_zero.is_nan(),
            "get_at must not be NaN with clamped decay"
        );
        assert_eq!(projected_zero, projected_min);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_zero_decay_add_is_safe() {
        let now = Instant::now();
        let mut ewma = Ewma::new(Duration::ZERO, now);

        ewma.add(5.0, now + Duration::from_secs(1));
        ewma.add(0.5, now + Duration::from_secs(2));

        let val = ewma.get();
        assert!(
            val.is_finite(),
            "add() result must be finite with clamped decay"
        );
        assert!(
            !val.is_nan(),
            "add() result must not be NaN with clamped decay"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_reset() {
        let now = Instant::now();
        let mut ewma = Ewma::new(Duration::from_secs(10), now);

        // Set state
        ewma.add(0.5, now + Duration::from_secs(1));
        assert_eq!(ewma.get(), 0.5);

        // Reset to a new value
        ewma.reset(1.0, now + Duration::from_secs(2));
        assert_eq!(ewma.get(), 1.0);

        // Verify EWMA continues working after reset.
        ewma.add(0.0, now + Duration::from_secs(12));
        assert_eq!(ewma.get(), EXP_NEG1);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_get_at_same_timestamp() {
        let now = Instant::now();
        let decay = Duration::from_secs(10);
        let add_at = now + Duration::from_secs(1);
        let read_at = add_at;

        let mut ewma = Ewma::new(decay, now);
        ewma.add(0.5, add_at);

        assert_eq!(ewma.get_at(read_at), 0.5);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_get_at_past_timestamp() {
        let now = Instant::now();
        let decay = Duration::from_secs(10);
        let add_at = now + Duration::from_secs(1);
        let read_at = now + Duration::from_millis(500);

        let mut ewma = Ewma::new(decay, now);
        ewma.add(0.5, add_at);

        assert_eq!(ewma.get_at(read_at), 0.5);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_get_at_infinity() {
        let now = Instant::now();
        let decay = Duration::from_secs(10);
        let probe_same = now;
        let probe_near = now + Duration::from_secs(1);
        let probe_far = now + Duration::from_secs(100);

        // A new Ewma without adding values should project INFINITY
        // at every timestamp.
        let ewma = Ewma::new(decay, now);
        assert_eq!(ewma.get_at(probe_same), f64::INFINITY);
        assert_eq!(ewma.get_at(probe_near), f64::INFINITY);
        assert_eq!(ewma.get_at(probe_far), f64::INFINITY);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_get_at_decay() {
        let now = Instant::now();
        let decay = Duration::from_secs(10);
        let add_at = now + Duration::from_secs(1);
        let read_at = now + Duration::from_secs(11);

        let mut ewma = Ewma::new(decay, now);
        ewma.add(1.0, add_at);

        // Verify that get_at applies value * exp(-elapsed/decay) correctly
        // without changing internal state.
        assert_eq!(ewma.get_at(read_at), EXP_NEG1);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_get_at_large_elapsed() {
        let now = Instant::now();
        let decay = Duration::from_secs(10);
        let add_at = now + Duration::from_secs(1);
        let read_at = now + Duration::from_secs(3600);

        let mut ewma = Ewma::new(decay, now);
        ewma.add(1.0, add_at);

        let result = ewma.get_at(read_at);
        assert!(
            result.is_finite(),
            "get_at at large elapsed must be finite, got {result}"
        );
        assert!(
            result < 1e-10,
            "get_at at large elapsed must be near zero, got {result}"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_get_at_non_mutation() {
        let now = Instant::now();
        let decay = Duration::from_secs(10);
        let first_add_at = now + Duration::from_secs(1);
        let read_at = now + Duration::from_secs(6);

        let mut ewma = Ewma::new(decay, now);
        ewma.add(1.0, first_add_at);

        // Take internal state before the read
        let value_before = ewma.value;
        let timestamp_before = ewma.timestamp;

        // Read must not mutate
        let _ = ewma.get_at(read_at);

        assert_eq!(ewma.value, value_before);
        assert_eq!(ewma.timestamp, timestamp_before);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_get_at_after_reset() {
        const EWMA_VAL: f64 = 0.5;
        const DECAY_AT_15S_VAL: f64 = EWMA_VAL * EXP_NEG1;

        let now = Instant::now();
        let decay = Duration::from_secs(10);
        let first_add_at = now + Duration::from_secs(1);
        let reset_at = now + Duration::from_secs(5);
        let immediate_read_at = reset_at;
        let decay_read_at = now + Duration::from_secs(15);

        let mut ewma = Ewma::new(decay, now);
        // Use a large value here to make sure we detect any issues with
        // reset not working properly, since we'd likely see a wildly
        // different value in the assert below.
        ewma.add(100.0, first_add_at);

        // Reset replaces both value and timestamp
        ewma.reset(EWMA_VAL, reset_at);

        // Must return the freshly-reset value
        assert_eq!(ewma.get_at(immediate_read_at), EWMA_VAL);

        // Read one decay period (10s) after the reset (so 15s decay).
        assert_eq!(ewma.get_at(decay_read_at), DECAY_AT_15S_VAL);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_get_at_between_adds() {
        const EXP_NEG0_5: f64 = 0.6065306597126334;

        let now = Instant::now();
        let decay = Duration::from_secs(10);
        let first_add_at = now + Duration::from_secs(1);
        let read_at = now + Duration::from_secs(6);
        let second_add_at = now + Duration::from_secs(11);

        let mut ewma = Ewma::new(decay, now);
        ewma.add(1.0, first_add_at);

        assert_eq!(ewma.get_at(read_at), EXP_NEG0_5);

        ewma.add(0.0, second_add_at);

        // Final state after the second add must be exp(-1.0),
        // ensuring get_at() doesn't change the internal state.
        assert_eq!(ewma.get(), EXP_NEG1);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "NaN")]
    async fn test_get_at_debug_asserts_nan() {
        let now = Instant::now();
        // Inject NaN via new_with_value
        let ewma = Ewma::new_with_value(Duration::from_secs(10), now, f64::NAN);

        // Should trigger a debug_assert
        let _ = ewma.get_at(now);
    }
}
