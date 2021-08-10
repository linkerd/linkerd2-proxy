use crate::{AllowPolicy, Config};
pub use futures::prelude::*;
use linkerd_app_core::{
    config,
    dns::Suffix,
    drain, exp_backoff, metrics,
    proxy::{
        http::{h1, h2},
        tap,
    },
    transport::{Keepalive, ListenAddr},
    NameMatch, ProxyRuntime,
};
pub use linkerd_app_test as support;
use linkerd_server_policy::{Authentication, Authorization, Protocol, ServerPolicy};
use std::time::Duration;

pub fn default_config() -> Config {
    let cluster_local = "svc.cluster.local."
        .parse::<Suffix>()
        .expect("`svc.cluster.local.` suffix is definitely valid");
    Config {
        allow_discovery: NameMatch::new(Some(cluster_local)),
        proxy: config::ProxyConfig {
            server: config::ServerConfig {
                addr: ListenAddr(([0, 0, 0, 0], 0).into()),
                keepalive: Keepalive(None),
                h2_settings: h2::Settings::default(),
            },
            connect: config::ConnectConfig {
                keepalive: Keepalive(None),
                timeout: Duration::from_secs(1),
                backoff: exp_backoff::ExponentialBackoff::new(
                    Duration::from_millis(100),
                    Duration::from_millis(500),
                    0.1,
                )
                .unwrap(),
                h1_settings: h1::PoolSettings {
                    max_idle: 1,
                    idle_timeout: Duration::from_secs(1),
                },
                h2_settings: h2::Settings::default(),
            },
            buffer_capacity: 10_000,
            cache_max_idle_age: Duration::from_secs(20),
            dispatch_timeout: Duration::from_secs(1),
            max_in_flight_requests: 10_000,
            detect_protocol_timeout: Duration::from_secs(10),
        },
        port_policies: AllowPolicy::new(ServerPolicy {
            protocol: Protocol::Detect {
                timeout: std::time::Duration::from_secs(10),
            },
            authorizations: vec![Authorization {
                authentication: Authentication::Unauthenticated,
                networks: vec![ipnet::Ipv4Net::default().into()],
                labels: Default::default(),
            }],
            labels: Default::default(),
        })
        .into(),
        profile_idle_timeout: Duration::from_millis(500),
    }
}

pub fn runtime() -> (ProxyRuntime, drain::Signal) {
    let (metrics, _) = metrics::Metrics::new(std::time::Duration::from_secs(10));
    let (drain_tx, drain) = drain::channel();
    let (tap, _) = tap::new();
    let runtime = ProxyRuntime {
        identity: None,
        metrics: metrics.outbound,
        tap,
        span_sink: None,
        drain,
    };
    (runtime, drain_tx)
}
