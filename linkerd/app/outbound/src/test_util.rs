use crate::Config;
pub use futures::prelude::*;
use linkerd_app_core::{
    config::{self, QueueConfig},
    drain, exp_backoff, metrics,
    proxy::{
        http::{h1, h2},
        tap,
    },
    transport::{DualListenAddr, Keepalive},
    IpMatch, IpNet, ProxyRuntime,
};
pub use linkerd_app_test as support;
use std::{str::FromStr, time::Duration};

pub(crate) fn default_config() -> Config {
    let buffer = QueueConfig {
        capacity: 10_000,
        failfast_timeout: Duration::from_secs(3),
    };
    Config {
        ingress_mode: false,
        emit_headers: true,
        allow_discovery: IpMatch::new(Some(IpNet::from_str("0.0.0.0/0").unwrap())).into(),
        proxy: config::ProxyConfig {
            server: config::ServerConfig {
                addr: DualListenAddr(([0, 0, 0, 0], 0).into(), None),
                keepalive: Keepalive(None),
                http2: h2::ServerParams::default(),
            },
            connect: config::ConnectConfig {
                keepalive: Keepalive(None),
                timeout: Duration::from_secs(1),
                backoff: exp_backoff::ExponentialBackoff::try_new(
                    Duration::from_millis(100),
                    Duration::from_millis(500),
                    0.1,
                )
                .unwrap(),
                http1: h1::PoolSettings {
                    max_idle: 1,
                    idle_timeout: Duration::from_secs(1),
                },
                http2: h2::ClientParams::default(),
            },
            max_in_flight_requests: 10_000,
            detect_protocol_timeout: Duration::from_secs(3),
        },
        inbound_ips: Default::default(),
        discovery_idle_timeout: Duration::from_secs(60),
        tcp_connection_queue: buffer,
        http_request_queue: buffer,
    }
}

pub(crate) fn runtime() -> (ProxyRuntime, drain::Signal) {
    let (drain_tx, drain) = drain::channel();
    let (tap, _) = tap::new();
    let (metrics, _) = metrics::Metrics::new(std::time::Duration::from_secs(10));
    let runtime = ProxyRuntime {
        identity: linkerd_meshtls_rustls::creds::default_for_test().1.into(),
        metrics: metrics.proxy,
        tap,
        span_sink: None,
        drain,
    };
    (runtime, drain_tx)
}
