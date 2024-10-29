use governor::{
    clock::DefaultClock,
    state::{keyed::DefaultKeyedStateStore, InMemoryState, NotKeyed, RateLimiter},
    Quota,
};
use std::num::NonZeroU32;

#[derive(Debug, Default)]
pub struct HttpLocalRateLimit {
    pub total: Option<RateLimiter<NotKeyed, InMemoryState, DefaultClock>>,
    pub identity: Option<RateLimiter<String, DefaultKeyedStateStore<String>, DefaultClock>>,
    pub overrides: Vec<HttpLocalRateLimitOverride>,
}

#[derive(Debug)]
pub struct HttpLocalRateLimitOverride {
    pub ids: Vec<String>,
    pub rate_limit: RateLimiter<Vec<String>, DefaultKeyedStateStore<Vec<String>>, DefaultClock>,
}

impl Default for HttpLocalRateLimitOverride {
    fn default() -> Self {
        Self {
            ids: vec![],
            rate_limit: RateLimiter::keyed(Quota::per_second(NonZeroU32::new(1).unwrap())),
        }
    }
}

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    use linkerd2_proxy_api::inbound::{self as api};

    impl From<api::HttpLocalRateLimit> for HttpLocalRateLimit {
        fn from(proto: api::HttpLocalRateLimit) -> Self {
            let total = proto.total.map(|lim| {
                let quota = Quota::per_second(NonZeroU32::new(lim.requests_per_second).unwrap());
                RateLimiter::direct(quota)
            });

            let identity = proto.identity.map(|lim| {
                let quota = Quota::per_second(NonZeroU32::new(lim.requests_per_second).unwrap());
                RateLimiter::keyed(quota)
            });

            let overrides = proto
                .overrides
                .into_iter()
                .flat_map(|ovr| {
                    ovr.limit.map(|lim| {
                        let ids = ovr
                            .clients
                            .into_iter()
                            .flat_map(|cl| cl.identities.into_iter().map(|id| id.name))
                            .collect();
                        let quota =
                            Quota::per_second(NonZeroU32::new(lim.requests_per_second).unwrap());
                        let rate_limit = RateLimiter::keyed(quota);
                        HttpLocalRateLimitOverride { ids, rate_limit }
                    })
                })
                .collect();

            Self {
                total,
                identity,
                overrides,
            }
        }
    }
}
