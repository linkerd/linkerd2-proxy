use std::{num::NonZeroU32, ops::RangeInclusive, time::Duration};

#[derive(Copy, Clone, Debug)]
pub struct Budget {
    ttl: Duration,
    min_per_sec: u32,
    ratio: f32,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct RoutePolicy<F> {
    /// Configures how responses are classified as retryable or non-retryable.
    pub retryable: F,
    /// Configures a per-request retry limit.
    pub max_per_request: Option<NonZeroU32>,
}

#[derive(Debug, thiserror::Error)]
pub enum InvalidRetryBudget {
    #[cfg(feature = "proto")]
    #[error("missing `ttl` field")]
    NoTtl,

    #[cfg(feature = "proto")]
    #[error("invalid `ttl` field: {0}")]
    BadDuration(#[from] prost_types::DurationError),

    #[error("retry ratio must be finite")]
    InfiniteRatio,

    #[error("`ttl` must be within {VALID_TTLS:?} (was {0:?})")]
    TtlOutOfRange(Duration),

    #[error("`retry_ratio` must be within {VALID_RATIOS:?} (was {0})")]
    PercentOutOfRange(f32),
}

// === impl Budget ===

const VALID_RATIOS: RangeInclusive<f32> = 1.0..=1000.0;
const VALID_TTLS: RangeInclusive<Duration> = Duration::from_secs(1)..=Duration::from_secs(60);

impl Budget {
    pub fn try_new(
        ttl: Duration,
        min_per_sec: u32,
        ratio: f32,
    ) -> Result<Self, InvalidRetryBudget> {
        if ratio.is_infinite() {
            return Err(InvalidRetryBudget::InfiniteRatio);
        }

        if !VALID_RATIOS.contains(&ratio) {
            return Err(InvalidRetryBudget::PercentOutOfRange(ratio));
        }

        if !VALID_TTLS.contains(&ttl) {
            return Err(InvalidRetryBudget::TtlOutOfRange(ttl));
        }

        Ok(Self {
            ttl,
            min_per_sec,
            ratio,
        })
    }

    #[inline]
    #[must_use]
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    #[inline]
    #[must_use]
    pub fn retry_ratio(&self) -> f32 {
        self.ratio
    }

    #[inline]
    #[must_use]
    pub fn min_per_sec(&self) -> u32 {
        self.min_per_sec
    }
}

impl PartialEq for Budget {
    fn eq(&self, other: &Self) -> bool {
        debug_assert!(self.ratio.is_finite());
        debug_assert!(other.ratio.is_finite());
        self.min_per_sec == other.min_per_sec && self.ratio == other.ratio && self.ttl == other.ttl
    }
}

// It's okay for `Budget` to be `Eq` because we assert that the
// ratio field (a float) is finite when constructing the backoff.
impl Eq for Budget {}

impl std::hash::Hash for Budget {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        debug_assert!(self.ratio.is_finite());
        self.min_per_sec.hash(state);
        self.ttl.hash(state);
        self.ratio.to_bits().hash(state);
    }
}

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    use linkerd2_proxy_api::destination;

    impl TryFrom<destination::RetryBudget> for Budget {
        type Error = InvalidRetryBudget;
        fn try_from(
            destination::RetryBudget {
                ttl,
                retry_ratio,
                min_retries_per_second,
            }: destination::RetryBudget,
        ) -> Result<Self, Self::Error> {
            let ttl = ttl.ok_or(InvalidRetryBudget::NoTtl)?;
            let ttl = ttl.try_into()?;
            Self::try_new(ttl, min_retries_per_second, retry_ratio)
        }
    }
}
