use crate::{endpoint::TcpLogical, Config};
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

const LOCALHOST: [u8; 4] = [127, 0, 0, 1];

fn default_config(orig_dst: SocketAddr) -> Config {
    Config {
        allow_discovery: IpMatch::new(Some(IpNet::from_str("0.0.0.0/0").unwrap())),
        canonicalize_timeout: Duration::from_millis(100),
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
            disable_protocol_detection_for_ports: Default::default(),
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
    let target_addr = SocketAddr::new([0, 0, 0, 0].into(), 666);
    let logical = TcpLogical {
        addr: target_addr,
        profile: None,
    };

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
        target_addr,
        target_addr,
        test_support::resolver::Metadata::default(),
    );

    // Build the outbound TCP balancer stack.
    let forward = cfg
        .build_tcp_balance(connect, resolver)
        .new_service(logical);

    forward
        .oneshot(client_io)
        .err_into::<Error>()
        .await
        .expect("conn should succeed");
}

#[tokio::test(core_threads = 1)]
async fn tls_when_hinted() {
    use super::TcpEndpoint;
    let _trace = test_support::trace_init();

    let tls_addr = SocketAddr::new([0, 0, 0, 0].into(), 5550);
    let (mut tx, rx) = tokio::sync::watch::channel(profiles::Profile::default());
    tokio::spawn(async move { tx.closed().await });
    let tls_logical = TcpLogical {
        addr: tls_addr,
        profile: Some(rx),
    };

    let plain_addr = SocketAddr::new([0, 0, 0, 0].into(), 5551);
    let plain_logical = TcpLogical {
        addr: plain_addr,
        profile: None,
    };

    let cfg = default_config(plain_addr);
    let id_name = linkerd2_identity::Name::from_hostname(
        b"foo.ns1.serviceaccount.identity.linkerd.cluster.local",
    )
    .expect("hostname is valid");
    let mut srv_io = test_support::io();
    srv_io.write(b"hello").read(b"world");
    let id_name2 = id_name.clone();

    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = test_support::connect()
        // The plaintext endpoint should use plaintext...
        .endpoint_builder(plain_addr, srv_io.clone())
        .endpoint_fn(tls_addr, move |endpoint: TcpEndpoint| {
            use tls::HasPeerIdentity;
            assert_eq!(
                endpoint.peer_identity(),
                tls::Conditional::Some(id_name2.clone())
            );
            let io = srv_io.build();
            Ok(io)
        });

    let tls_meta = test_support::resolver::Metadata::new(
        Default::default(),
        test_support::resolver::ProtocolHint::Unknown,
        Some(id_name),
        10_000,
        None,
    );

    // Configure the mock destination resolver to just give us a single endpoint
    // for the target, which always exists and has no metadata.
    let resolver = test_support::resolver()
        .endpoint_exists(
            plain_addr,
            plain_addr,
            test_support::resolver::Metadata::default(),
        )
        .endpoint_exists(tls_addr, tls_addr, tls_meta);

    // Configure mock IO for the "client".
    let mut client_io = test_support::io();
    client_io.read(b"hello").write(b"world");

    // Build the outbound TCP balancer stack.
    let mut balance = cfg.build_tcp_balance(connect, resolver);

    let plain = balance
        .new_service(plain_logical)
        .oneshot(client_io.build())
        .err_into::<Error>();
    let tls = balance
        .new_service(tls_logical)
        .oneshot(client_io.build())
        .err_into::<Error>();

    let res = tokio::try_join! { plain, tls };
    res.expect("neither connection should fail");
}
