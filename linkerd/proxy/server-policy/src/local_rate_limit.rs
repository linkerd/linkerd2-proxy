#[cfg(test)]
use governor::clock::FakeRelativeClock;
use governor::{
    clock::{Clock, DefaultClock},
    middleware::NoOpMiddleware,
    state::{keyed::HashMapStateStore, InMemoryState, RateLimiter, StateStore},
};
use linkerd_identity::Id;
use std::{collections::HashMap, num::NonZeroU32, sync::Arc};

#[cfg(test)]
mod tests;

type Direct = InMemoryState;
type Keyed = HashMapStateStore<Option<Id>>;

#[derive(Debug, Default)]
pub struct LocalRateLimit<C: Clock = DefaultClock> {
    total: Option<RateLimit<Direct, C>>,
    per_identity: Option<RateLimit<Keyed, C>>,
    overrides: HashMap<Id, Arc<RateLimit<Direct, C>>>,
}

#[derive(Debug)]
struct RateLimit<S, C = DefaultClock>
where
    S: StateStore,
    C: Clock,
{
    rps: NonZeroU32,
    limiter: RateLimiter<S::Key, S, C, NoOpMiddleware<C::Instant>>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RateLimitError {
    #[error("total rate limit exceeded: {0}rps")]
    Total(NonZeroU32),
    #[error("per-identity rate limit exceeded: {0}rps")]
    PerIdentity(NonZeroU32),
    #[error("override rate limit exceeded: {0}rps")]
    Override(NonZeroU32),
}

// === impl LocalRateLimit ===

#[cfg(any(feature = "proto", feature = "test-util"))]
impl RateLimit<Direct, DefaultClock> {
    fn direct(rps: NonZeroU32) -> Self {
        let limiter = RateLimiter::direct(governor::Quota::per_second(rps));
        Self { rps, limiter }
    }
}

#[cfg(any(feature = "proto", feature = "test-util"))]
impl RateLimit<Keyed, DefaultClock> {
    fn keyed(rps: NonZeroU32) -> Self {
        let limiter = RateLimiter::hashmap(governor::Quota::per_second(rps));
        Self { rps, limiter }
    }
}

#[cfg(feature = "test-util")]
impl LocalRateLimit {
    pub fn new_no_overrides_for_test(
        total: Option<u32>,
        per_identity: Option<u32>,
    ) -> LocalRateLimit<DefaultClock> {
        LocalRateLimit {
            total: total.and_then(NonZeroU32::new).map(RateLimit::direct),
            per_identity: per_identity.and_then(NonZeroU32::new).map(RateLimit::keyed),
            overrides: HashMap::new(),
        }
    }
}

impl<C: Clock> LocalRateLimit<C> {
    pub fn check(&self, id: Option<&Id>) -> Result<(), RateLimitError> {
        if let Some(lim) = &self.total {
            if lim.limiter.check().is_err() {
                return Err(RateLimitError::Total(lim.rps));
            }
        }

        if let Some(id) = id {
            if let Some(lim) = self.overrides.get(id) {
                if lim.limiter.check().is_err() {
                    return Err(RateLimitError::Override(lim.rps));
                }
                return Ok(());
            }
        }

        if let Some(lim) = &self.per_identity {
            // Note that clients with no identity share the same rate limit (Id = None)
            if lim.limiter.check_key(&id.cloned()).is_err() {
                return Err(RateLimitError::PerIdentity(lim.rps));
            }
        }

        Ok(())
    }
}

// === impl RateLimit ===

#[cfg(test)]
impl RateLimit<Direct, FakeRelativeClock> {
    fn direct_for_test(rps: u32) -> Self {
        let rps = NonZeroU32::new(rps).expect("non-zero RPS");
        let quota = governor::Quota::per_second(rps);
        let limiter = RateLimiter::direct_with_clock(quota, FakeRelativeClock::default());

        Self { rps, limiter }
    }
}

#[cfg(test)]
impl RateLimit<Keyed, FakeRelativeClock> {
    fn keyed_for_test(rps: u32) -> Self {
        let rps = NonZeroU32::new(rps).expect("non-zero RPS");
        let quota = governor::Quota::per_second(rps);
        let limiter = RateLimiter::hashmap_with_clock(quota, FakeRelativeClock::default());

        Self { rps, limiter }
    }
}

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    use linkerd2_proxy_api::inbound as api;

    impl From<api::HttpLocalRateLimit> for LocalRateLimit {
        fn from(proto: api::HttpLocalRateLimit) -> Self {
            // Zero-value
            let total = proto
                .total
                .and_then(|l| NonZeroU32::new(l.requests_per_second))
                .map(RateLimit::direct);
            let per_identity = proto
                .identity
                .and_then(|l| NonZeroU32::new(l.requests_per_second))
                .map(RateLimit::keyed);
            let overrides = proto
                .overrides
                .into_iter()
                .flat_map(|ovr| {
                    let Some(limiter) = ovr
                        .limit
                        .and_then(|l| NonZeroU32::new(l.requests_per_second))
                        .map(RateLimit::direct)
                    else {
                        return vec![];
                    };
                    let limit = Arc::new(limiter);
                    ovr.clients
                        .into_iter()
                        .flat_map(|cl| {
                            cl.identities
                                .into_iter()
                                .filter_map(|id| id.name.parse::<Id>().ok())
                        })
                        .map(move |id| (id, limit.clone()))
                        .collect()
                })
                .collect();

            Self {
                total,
                per_identity,
                overrides,
            }
        }
    }
}
