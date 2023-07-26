use std::{num::NonZeroU32, sync::Arc};

#[derive(Clone, Debug)]
pub struct Budget(Arc<tower::retry::budget::Budget>);

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct RoutePolicy<F> {
    /// Configures how responses are classified as retryable or non-retryable.
    pub retryable: F,
    /// Configures a per-request retry limit.
    pub max_per_request: Option<NonZeroU32>,
}

// === impl Budget ===

impl PartialEq for Budget {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

// It's okay for `Budget` to be `Eq` because we assert that the
// ratio field (a float) is finite when constructing the backoff.
impl Eq for Budget {}

impl std::hash::Hash for Budget {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_usize(Arc::as_ref(&self.0) as *const _ as usize);
    }
}

impl From<Budget> for Arc<tower::retry::budget::Budget> {
    fn from(Budget(this): Budget) -> Self {
        this
    }
}

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    use linkerd2_proxy_api::destination;
    use std::{ops::RangeInclusive, time::Duration};

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidRetryBudget {
        #[error("missing `ttl` field")]
        NoTtl,

        #[error("invalid `ttl` field: {0}")]
        BadDuration(#[from] prost_types::DurationError),

        #[error("retry ratio must be finite")]
        InfiniteRatio,

        #[error("`ttl` must be within {VALID_TTLS:?} (was {0:?})")]
        TtlOutOfRange(Duration),

        #[error("`retry_ratio` must be within {VALID_RATIOS:?} (was {0})")]
        PercentOutOfRange(f32),
    }

    const VALID_RATIOS: RangeInclusive<f32> = 1.0..=1000.0;
    const VALID_TTLS: RangeInclusive<Duration> = Duration::from_secs(1)..=Duration::from_secs(60);

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

            if retry_ratio.is_infinite() {
                return Err(InvalidRetryBudget::InfiniteRatio);
            }

            if !VALID_RATIOS.contains(&retry_ratio) {
                return Err(InvalidRetryBudget::PercentOutOfRange(retry_ratio));
            }

            if !VALID_TTLS.contains(&ttl) {
                return Err(InvalidRetryBudget::TtlOutOfRange(ttl));
            }

            Ok(Self(Arc::new(tower::retry::budget::Budget::new(
                ttl,
                min_retries_per_second,
                retry_ratio,
            ))))
        }
    }
}
