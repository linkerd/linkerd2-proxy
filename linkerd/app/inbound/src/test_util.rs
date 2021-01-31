use crate::{Config, RequireIdentityForPorts, SkipByPort};
pub use futures::prelude::*;
use linkerd_app_core::{
    config,
    dns::Suffix,
    exp_backoff,
    proxy::http::{h1, h2},
    transport::BindTcp,
    NameMatch,
};
pub use linkerd_app_test as support;
use std::{net::SocketAddr, time::Duration};

const LOCALHOST: [u8; 4] = [127, 0, 0, 1];

pub fn default_config(orig_dst: SocketAddr) -> Config {
    let cluster_local = "svc.cluster.local."
        .parse::<Suffix>()
        .expect("`svc.cluster.local.` suffix is definitely valid");
    Config {
        allow_discovery: NameMatch::new(Some(cluster_local)),
        proxy: config::ProxyConfig {
            server: config::ServerConfig {
                bind: BindTcp::new(SocketAddr::new(LOCALHOST.into(), 0), None)
                    .with_orig_dst_addr(orig_dst.into()),
                h2_settings: h2::Settings::default(),
            },
            connect: config::ConnectConfig {
                keepalive: None,
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
        require_identity_for_inbound_ports: RequireIdentityForPorts::from(None),
        disable_protocol_detection_for_ports: SkipByPort::from(indexmap::IndexSet::default()),
        profile_idle_timeout: Duration::from_millis(500),
    }
}
