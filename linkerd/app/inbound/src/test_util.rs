use crate::{policy, Config};
pub use futures::prelude::*;
use linkerd_app_core::{
    config::{self, QueueConfig},
    control,
    dns::Suffix,
    drain, exp_backoff,
    identity::rustls,
    metrics,
    proxy::{
        http::{h1, h2},
        tap,
    },
    tls,
    transport::{Keepalive, ListenAddr},
    ProxyRuntime,
};
pub use linkerd_app_test as support;
use std::time::Duration;

pub fn default_config() -> Config {
    let cluster_local = "svc.cluster.local."
        .parse::<Suffix>()
        .expect("`svc.cluster.local.` suffix is definitely valid");
    let connect = config::ConnectConfig {
        keepalive: Keepalive(None),
        timeout: Duration::from_secs(1),
        backoff: exp_backoff::ExponentialBackoff::try_new(
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
    };

    let policy = policy::Config {
        control: control::Config {
            addr: control::ControlAddr {
                addr: "policy.linkerd.svc.cluster.local:8090"
                    .parse()
                    .expect("control addr must be valid"),
                identity: tls::ConditionalClientTls::Some(tls::ClientTls {
                    server_id: "policy.linkerd.serviceaccount.identity.linkerd.cluster.local"
                        .parse()
                        .expect("control identity name must be valid"),
                    alpn: None,
                }),
            },
            connect: connect.clone(),
            buffer: QueueConfig {
                capacity: 10_000,
                failfast_timeout: Duration::from_secs(10),
            },
        },
        cache_max_idle_age: Duration::from_secs(20),
        workload: "test".into(),
        ports: Default::default(),
    };

    Config {
        policy,
        allow_discovery: Some(cluster_local).into_iter().collect(),
        proxy: config::ProxyConfig {
            server: config::ServerConfig {
                addr: ListenAddr(([0, 0, 0, 0], 0).into()),
                keepalive: Keepalive(None),
                h2_settings: h2::Settings::default(),
            },
            connect,
            max_in_flight_requests: 10_000,
            detect_protocol_timeout: Duration::from_secs(10),
        },
        allowed_ips: Default::default(),
        http_request_queue: config::QueueConfig {
            capacity: 10_000,
            failfast_timeout: Duration::from_secs(1),
        },
        discovery_idle_timeout: Duration::from_secs(20),
        profile_skip_timeout: Duration::from_secs(1),
    }
}

pub fn runtime() -> (ProxyRuntime, drain::Signal) {
    let (drain_tx, drain) = drain::channel();
    let (tap, _) = tap::new();
    let (metrics, _) =
        metrics::Metrics::new(std::time::Duration::from_secs(10), Default::default());
    let runtime = ProxyRuntime {
        identity: rustls::creds::default_for_test().1.into(),
        metrics: metrics.proxy,
        tap,
        span_sink: None,
        drain,
    };
    (runtime, drain_tx)
}
