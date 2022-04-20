#![deny(
    warnings,
    rust_2018_idioms,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]
#![forbid(unsafe_code)]

use futures::Stream;
use pin_project::pin_project;
use rand::{rngs::SmallRng, thread_rng, SeedableRng};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use thiserror::Error;
use tokio::time;

/// A jittered exponential backoff strategy.
#[derive(Copy, Clone, Debug, Default)]
pub struct ExponentialBackoff {
    /// The minimum amount of time to wait before resuming an operation.
    min: time::Duration,

    /// The maximum amount of time to wait before resuming an operation.
    max: time::Duration,

    /// The ratio of the base timeout that may be randomly added to a backoff.
    ///
    /// Must be greater than or equal to 0.0.
    jitter: f64,
}

/// A jittered exponential backoff stream.
#[pin_project]
#[derive(Debug)]
pub struct ExponentialBackoffStream {
    backoff: ExponentialBackoff,
    rng: SmallRng,
    iterations: u32,
    sleeping: bool,
    sleep: Pin<Box<time::Sleep>>,
}

#[derive(Clone, Debug, Error)]
#[error("invalid backoff: {0}")]
pub struct InvalidBackoff(&'static str);

impl ExponentialBackoff {
    pub const fn new_unchecked(min: time::Duration, max: time::Duration, jitter: f64) -> Self {
        Self { min, max, jitter }
    }

    pub fn try_new(
        min: time::Duration,
        max: time::Duration,
        jitter: f64,
    ) -> Result<Self, InvalidBackoff> {
        if min > max {
            return Err(InvalidBackoff("maximum must not be less than minimum"));
        }
        if max == time::Duration::from_millis(0) {
            return Err(InvalidBackoff("maximum must be non-zero"));
        }
        if jitter < 0.0 {
            return Err(InvalidBackoff("jitter must not be negative"));
        }
        if jitter > 100.0 {
            return Err(InvalidBackoff("jitter must not be greater than 100"));
        }
        if !jitter.is_finite() {
            return Err(InvalidBackoff("jitter must be finite"));
        }
        Ok(ExponentialBackoff { min, max, jitter })
    }

    pub fn stream(&self) -> ExponentialBackoffStream {
        ExponentialBackoffStream {
            backoff: *self,
            rng: SmallRng::from_rng(&mut thread_rng()).expect("RNG must be valid"),
            iterations: 0,
            sleeping: false,
            sleep: Box::pin(time::sleep(time::Duration::from_secs(0))),
        }
    }

    fn base(&self, iterations: u32) -> time::Duration {
        debug_assert!(
            self.min <= self.max,
            "maximum backoff must not be less than minimum backoff"
        );
        debug_assert!(
            self.max > time::Duration::from_millis(0),
            "Maximum backoff must be non-zero"
        );
        self.min
            .checked_mul(2_u32.saturating_pow(iterations))
            .unwrap_or(self.max)
            .min(self.max)
    }

    /// Returns a random, uniform duration on `[0, base*self.jitter]` no greater
    /// than `self.max`.
    fn jitter<R: rand::Rng>(&self, base: time::Duration, rng: &mut R) -> time::Duration {
        if self.jitter == 0.0 {
            time::Duration::default()
        } else {
            let jitter_factor = rng.gen::<f64>();
            debug_assert!(
                jitter_factor > 0.0,
                "rng returns values between 0.0 and 1.0"
            );
            let rand_jitter = jitter_factor * self.jitter;
            let secs = (base.as_secs() as f64) * rand_jitter;
            let nanos = (base.subsec_nanos() as f64) * rand_jitter;
            let remaining = self.max - base;
            time::Duration::new(secs as u64, nanos as u32).min(remaining)
        }
    }
}

impl Stream for ExponentialBackoffStream {
    type Item = ();

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        loop {
            // If there's an active delay, wait until it's done and then
            // update the state.
            if *this.sleeping {
                futures::ready!(this.sleep.as_mut().poll(cx));

                *this.sleeping = false;
                *this.iterations += 1;
                return Poll::Ready(Some(()));
            }
            if *this.iterations == u32::MAX {
                return Poll::Ready(None);
            }

            let backoff = {
                let base = this.backoff.base(*this.iterations);
                base + this.backoff.jitter(base, &mut this.rng)
            };
            this.sleep.as_mut().reset(time::Instant::now() + backoff);
            *this.sleeping = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quickcheck::*;

    quickcheck! {
        fn backoff_base_first(min_ms: u64, max_ms: u64) -> TestResult {
            let min = time::Duration::from_millis(min_ms);
            let max = time::Duration::from_millis(max_ms);
            let backoff = match ExponentialBackoff::try_new(min, max, 0.0) {
                Err(_) => return TestResult::discard(),
                Ok(backoff) => backoff,
            };
            let delay = backoff.base(0);
            TestResult::from_bool(min == delay)
        }

        fn backoff_base(min_ms: u64, max_ms: u64, iterations: u32) -> TestResult {
            let min = time::Duration::from_millis(min_ms);
            let max = time::Duration::from_millis(max_ms);
            let backoff = match ExponentialBackoff::try_new(min, max, 0.0) {
                Err(_) => return TestResult::discard(),
                Ok(backoff) => backoff,
            };
            let delay = backoff.base(iterations);
            TestResult::from_bool(min <= delay && delay <= max)
        }

        fn backoff_jitter(base_ms: u64, max_ms: u64, jitter: f64) -> TestResult {
            let base = time::Duration::from_millis(base_ms);
            let max = time::Duration::from_millis(max_ms);
            let backoff = match ExponentialBackoff::try_new(base, max, jitter) {
                Err(_) => return TestResult::discard(),
                Ok(backoff) => backoff,
            };

            let j = backoff.jitter(base, &mut rand::thread_rng());
            if jitter == 0.0 || base_ms == 0 || max_ms == base_ms {
                TestResult::from_bool(j == time::Duration::default())
            } else {
                TestResult::from_bool(j > time::Duration::default())
            }
        }
    }
}
