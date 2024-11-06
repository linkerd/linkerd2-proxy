use super::*;
use maplit::hashmap;
use std::time::Duration;

#[cfg(feature = "proto")]
#[tokio::test(flavor = "current_thread")]
async fn from_proto() {
    use linkerd2_proxy_api::{
        inbound::{self, http_local_rate_limit},
        meta,
    };

    let client_1: Id = "client-1".parse().unwrap();
    let client_2: Id = "client-2".parse().unwrap();
    let client_3: Id = "client-3".parse().unwrap();
    let client_4: Id = "client-4".parse().unwrap();
    let rl_proto = inbound::HttpLocalRateLimit {
        metadata: Some(meta::Metadata {
            kind: Some(meta::metadata::Kind::Default("ratelimit-1".into())),
        }),
        total: Some(http_local_rate_limit::Limit {
            requests_per_second: 100,
        }),
        identity: Some(http_local_rate_limit::Limit {
            requests_per_second: 20,
        }),
        overrides: vec![
            http_local_rate_limit::Override {
                limit: Some(http_local_rate_limit::Limit {
                    requests_per_second: 50,
                }),
                clients: Some(http_local_rate_limit::r#override::ClientIdentities {
                    identities: vec![
                        inbound::Identity {
                            name: client_1.to_string(),
                        },
                        inbound::Identity {
                            name: client_2.to_string(),
                        },
                    ],
                }),
            },
            http_local_rate_limit::Override {
                limit: Some(http_local_rate_limit::Limit {
                    requests_per_second: 75,
                }),
                clients: Some(http_local_rate_limit::r#override::ClientIdentities {
                    identities: vec![
                        inbound::Identity {
                            name: client_3.to_string(),
                        },
                        inbound::Identity {
                            name: client_4.to_string(),
                        },
                    ],
                }),
            },
        ],
    };

    let rl = Into::<LocalRateLimit>::into(rl_proto);
    assert_eq!(rl.total.as_ref().unwrap().rps.get(), 100);
    assert_eq!(rl.per_identity.as_ref().unwrap().rps.get(), 20);

    assert_eq!(rl.overrides.get(&client_1).unwrap().rps.get(), 50);
    assert_eq!(rl.overrides.get(&client_2).unwrap().rps.get(), 50);
    assert_eq!(rl.overrides.get(&client_3).unwrap().rps.get(), 75);
    assert_eq!(rl.overrides.get(&client_4).unwrap().rps.get(), 75);
}

#[tokio::test(flavor = "current_thread")]
async fn check_rate_limits() {
    let total = RateLimit::direct_for_test(35);
    let per_identity = RateLimit::keyed_for_test(5);
    let overrides = hashmap! {
        "client-3".parse().unwrap() => Arc::new(RateLimit::direct_for_test(10)),
        "client-4".parse().unwrap() => Arc::new(RateLimit::direct_for_test(15)),
    };
    let rl = LocalRateLimit {
        total: Some(total),
        per_identity: Some(per_identity),
        overrides,
    };

    // These clients will be rate-limited by the per_identity rate-limiter
    let client_1: Id = "client-1".parse().unwrap();
    let client_2: Id = "client-2".parse().unwrap();

    // These clients will be rate-limited by the overrides rate-limiters
    let client_3: Id = "client-3".parse().unwrap();
    let client_4: Id = "client-4".parse().unwrap();

    let total_clock = rl.total.as_ref().unwrap().limiter.clock();
    let per_identity_clock = rl.per_identity.as_ref().unwrap().limiter.clock();
    let client_3_clock = rl.overrides.get(&client_3).unwrap().limiter.clock();
    let client_4_clock = rl.overrides.get(&client_4).unwrap().limiter.clock();

    // This loop checks that:
    // - client_1 gets rate-limited first via the per_identity rate-limiter
    // - then client_3 and client_4 via the overrides rate-limiters
    // - then client_2 via the total rate-limiter
    // - then we advance time to replenish the rate-limiters buckets and repeat the checks
    for _ in 1..=5 {
        // Requests per-client: 5
        // Total requests: 15
        // All clients should NOT be rate-limited
        for _ in 1..=5 {
            assert!(rl.check(Some(&client_1)).is_ok());
            assert!(rl.check(Some(&client_3)).is_ok());
            assert!(rl.check(Some(&client_4)).is_ok());
        }
        // Reached per_identity limit for client_1
        // Total requests: 16
        assert_eq!(
            rl.check(Some(&client_1)),
            Err(RateLimitError::PerIdentity(NonZeroU32::new(5).unwrap()))
        );

        // Requests per-client: 10
        // Total requests thus far: 26
        // client_3 and client_4 should NOT be rate-limited
        for _ in 1..=5 {
            assert!(rl.check(Some(&client_3)).is_ok());
            assert!(rl.check(Some(&client_4)).is_ok());
        }
        // Total requests thus far: 27
        // Reached override limit for client_3
        assert_eq!(
            rl.check(Some(&client_3)),
            Err(RateLimitError::Override(NonZeroU32::new(10).unwrap()))
        );

        // Requests per-client: 5
        // Total requests thus far: 32
        // client_4 should NOT be rate-limited
        for _ in 1..=5 {
            assert!(rl.check(Some(&client_4)).is_ok());
        }
        // Total requests: 33
        // Reached override limit for client_4
        assert_eq!(
            rl.check(Some(&client_4)),
            Err(RateLimitError::Override(NonZeroU32::new(15).unwrap()))
        );

        // Total requests: 35
        // Only 2 requests for client_2 allowed as we're reaching the total rate-limit
        for _ in 1..=2 {
            assert!(rl.check(Some(&client_2)).is_ok());
        }

        // Total requests: 36
        // Reached total limit for all clients
        assert_eq!(
            rl.check(Some(&client_2)),
            Err(RateLimitError::Total(NonZeroU32::new(35).unwrap()))
        );

        // Advance time for a seconds to replenish the rate-limiters buckets
        total_clock.advance(Duration::from_secs(1));
        per_identity_clock.advance(Duration::from_secs(1));
        client_3_clock.advance(Duration::from_secs(1));
        client_4_clock.advance(Duration::from_secs(1));
    }
}
