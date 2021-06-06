use crate::Config;
pub use futures::prelude::*;
pub use ipnet::IpNet;
use linkerd_app_core::{
    config, drain, exp_backoff, metrics,
    proxy::{
        http::{h1, h2},
        tap,
    },
    transport::{Keepalive, ListenAddr},
    IpMatch, ProxyRuntime,
};
pub use linkerd_app_test as support;
use std::{str::FromStr, time::Duration};

pub fn default_config() -> Config {
    Config {
        ingress_mode: false,
        allow_discovery: IpMatch::new(Some(IpNet::from_str("0.0.0.0/0").unwrap())).into(),
        max_retry_size_bytes: 64 * 1024,
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
            cache_max_idle_age: Duration::from_secs(60),
            dispatch_timeout: Duration::from_secs(3),
            max_in_flight_requests: 10_000,
            detect_protocol_timeout: Duration::from_secs(3),
        },
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
