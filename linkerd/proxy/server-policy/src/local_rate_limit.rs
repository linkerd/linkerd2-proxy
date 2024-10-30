use governor::{
    clock::DefaultClock,
    state::{keyed::HashMapStateStore, InMemoryState, RateLimiter, StateStore},
};
use linkerd_identity::Id;
use std::{collections::HashMap, num::NonZeroU32, sync::Arc};

#[derive(Debug, Default)]
pub struct LocalRateLimit {
    total: Option<RateLimit<InMemoryState>>,
    per_identity: Option<RateLimit<HashMapStateStore<Option<Id>>>>,
    overrides: HashMap<Id, Arc<RateLimit<InMemoryState>>>,
}
#[derive(Debug)]
struct RateLimit<S: StateStore> {
    rps: NonZeroU32,
    limiter: RateLimiter<S::Key, S, DefaultClock>,
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

impl LocalRateLimit {
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
            if lim.limiter.check_key(&id.cloned()).is_err() {
                return Err(RateLimitError::PerIdentity(lim.rps));
            }
        }

        Ok(())
    }
}

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    use governor::Quota;
    use linkerd2_proxy_api::inbound as api;

    impl From<api::HttpLocalRateLimit> for LocalRateLimit {
        fn from(proto: api::HttpLocalRateLimit) -> Self {
            let total = proto.total.and_then(|lim| {
                let rps = NonZeroU32::new(lim.requests_per_second)?;
                let limiter = RateLimiter::direct(Quota::per_second(rps));
                Some(RateLimit { rps, limiter })
            });
            let per_identity = proto.identity.and_then(|lim| {
                let rps = NonZeroU32::new(lim.requests_per_second)?;
                let limiter = RateLimiter::hashmap(Quota::per_second(rps));
                Some(RateLimit { rps, limiter })
            });
            let overrides = proto
                .overrides
                .into_iter()
                .flat_map(|ovr| {
                    let Some(lim) = ovr.limit else {
                        return vec![];
                    };
                    let Some(rps) = NonZeroU32::new(lim.requests_per_second) else {
                        return vec![];
                    };
                    let limiter = RateLimiter::direct(Quota::per_second(rps));
                    let limit = Arc::new(RateLimit { rps, limiter });
                    ovr.clients
                        .into_iter()
                        .flat_map(|cl| {
                            cl.identities
                                .into_iter()
                                .filter_map(|id| id.name.parse::<Id>().ok())
                        })
                        .map(move |id| (id, limit.clone()))
                        .collect::<Vec<(Id, Arc<RateLimit<InMemoryState>>)>>()
                })
                .collect::<HashMap<Id, Arc<RateLimit<InMemoryState>>>>();

            Self {
                total,
                per_identity,
                overrides,
            }
        }
    }
}
