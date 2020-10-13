use crate::{
    endpoint::{TcpConcrete, TcpLogical},
    Config,
};
use futures::prelude::*;
use ipnet::IpNet;
use linkerd2_app_core::{
    config, exp_backoff, profiles,
    proxy::http::h2,
    svc::NewService,
    transport::{listen, tls},
    Error, IpMatch,
};
use linkerd2_app_test as test_support;
use std::{net::SocketAddr, str::FromStr, time::Duration};
use tower::ServiceExt;

mod tcp;

const LOCALHOST: [u8; 4] = [127, 0, 0, 1];

fn profile() -> profiles::Receiver {
    let (mut tx, rx) = tokio::sync::watch::channel(profiles::Profile::default());
    tokio::spawn(async move { tx.closed().await });
    rx
}

fn default_config(orig_dst: SocketAddr) -> Config {
    Config {
        allow_discovery: IpMatch::new(Some(IpNet::from_str("0.0.0.0/0").unwrap())),
        proxy: config::ProxyConfig {
            server: config::ServerConfig {
                bind: listen::Bind::new(SocketAddr::new(LOCALHOST.into(), 0), None)
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
