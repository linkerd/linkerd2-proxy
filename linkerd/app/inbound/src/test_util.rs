use crate::{policy, Config};
pub use futures::prelude::*;
use linkerd_app_core::{
    config,
    dns::Suffix,
    drain, exp_backoff,
    identity::rustls,
    metrics,
    proxy::{
        http::{h1, h2},
        tap,
    },
    transport::{Keepalive, ListenAddr},
    ProxyRuntime,
};
pub use linkerd_app_test as support;
use linkerd_proxy_server_policy::{Authentication, Authorization, Meta, Protocol, ServerPolicy};
use std::{sync::Arc, time::Duration};

pub fn default_config() -> Config {
    let cluster_local = "svc.cluster.local."
        .parse::<Suffix>()
        .expect("`svc.cluster.local.` suffix is definitely valid");

    let authorizations = Arc::new([Authorization {
        authentication: Authentication::Unauthenticated,
        networks: vec![Default::default()],
        meta: Arc::new(Meta::Resource {
            group: "policy.linkerd.io".into(),
            kind: "serverauthorization".into(),
            name: "testsaz".into(),
        }),
    }]);
    let policy = policy::Config::Fixed {
        cache_max_idle_age: Duration::from_secs(20),
        default: ServerPolicy {
            protocol: Protocol::Detect {
                timeout: std::time::Duration::from_secs(10),
                http: Arc::new([linkerd_proxy_server_policy::http::default(
                    authorizations.clone(),
                )]),
                tcp_authorizations: authorizations,
            },
            meta: Arc::new(Meta::Resource {
                group: "policy.linkerd.io".into(),
                kind: "server".into(),
                name: "testsrv".into(),
            }),
        }
        .into(),
        ports: Default::default(),
        opaque_ports: Default::default(),
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
            connect: config::ConnectConfig {
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
            },
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
    let (metrics, _) = metrics::Metrics::new(std::time::Duration::from_secs(10));
    let runtime = ProxyRuntime {
        identity: rustls::creds::default_for_test().1.into(),
        metrics: metrics.proxy,
        tap,
        span_sink: None,
        drain,
    };
    (runtime, drain_tx)
}
