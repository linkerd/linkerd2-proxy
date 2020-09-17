use crate::Config;
use futures::prelude::*;
use indexmap::indexset;
use linkerd2_app_core::{self as app_core, Addr, Error};
use linkerd2_app_test as test_support;
use std::{net::SocketAddr, time::Duration};
use tower::ServiceExt;

const LOCALHOST: [u8; 4] = [127, 0, 0, 1];
const LISTEN_PORT: u16 = 4140;

fn default_config(orig_dst: SocketAddr) -> Config {
    use app_core::{
        config::{ConnectConfig, ProxyConfig, ServerConfig},
        exp_backoff::ExponentialBackoff,
        proxy::http::h2,
        transport::listen,
    };
    let h2_settings = h2::Settings {
        initial_stream_window_size: Some(65_535), // Protocol default
        initial_connection_window_size: Some(1_048_576), // 1MB ~ 16 streams at capacity
    };
    Config {
        canonicalize_timeout: Duration::from_millis(100),
        proxy: ProxyConfig {
            server: ServerConfig {
                bind: listen::Bind::new(SocketAddr::new(LOCALHOST.into(), LISTEN_PORT), None)
                    .with_orig_dst_addr(orig_dst.into()),
                h2_settings,
            },
            connect: ConnectConfig {
                keepalive: None,
                timeout: Duration::from_secs(1),
                backoff: ExponentialBackoff::new(
                    Duration::from_millis(100),
                    Duration::from_millis(500),
                    0.1,
                )
                .unwrap(),
                h2_settings,
            },
            buffer_capacity: 10_000,
            cache_max_idle_age: Duration::from_secs(60),
            disable_protocol_detection_for_ports: indexset![
                25,   // SMTP
                587,  // SMTP
                3306, // MySQL
            ]
            .into(),
            dispatch_timeout: Duration::from_secs(3),
            max_in_flight_requests: 10_000,
            detect_protocol_timeout: Duration::from_secs(3),
        },
    }
}

#[tokio::test(core_threads = 1)]
async fn plaintext_tcp() {
    let _trace = test_support::trace_init();

    // Since all of the actual IO in this test is mocked out, we won't actually
    // bind any of these addresses. Therefore, we don't need to use ephemeral
    // ports or anything. These will just be used so that the proxy has a socket
    // address to resolve, etc.
    let target_addr = SocketAddr::new([0, 0, 0, 0].into(), 0);

    let cfg = default_config(target_addr);

    // Configure mock IO for the upstream "server". It will read "hello" and
    // then write "world".
    let mut srv_io = test_support::io();
    srv_io.read(b"hello").write(b"world");
    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = test_support::connect().endpoint_builder(target_addr, srv_io);

    // Configure mock IO for the "client".
    let client_io = test_support::io().write(b"hello").read(b"world").build();

    // Configure the mock destination resolver to just give us a single endpoint
    // for the target, which always exists and has no metadata.
    let resolver = test_support::resolver().endpoint_exists(
        Addr::from(target_addr),
        target_addr,
        test_support::resolver::Metadata::empty(),
    );

    // Build the outbound TCP balancer stack.
    cfg.build_tcp_balance(connect, resolver)
        .oneshot(target_addr)
        .err_into::<Error>()
        .await
        .expect("make service should succeed")
        .oneshot(client_io)
        .err_into::<Error>()
        .await
        .expect("conn should succeed");
}
