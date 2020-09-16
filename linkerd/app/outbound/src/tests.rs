use crate::Config;
use indexmap::indexset;
use linkerd2_app_core::{self as app_core, metrics::Metrics, transport::tls, Addr};
use linkerd2_app_test::{self as test_support, io, TestMakeServiceExt};
use std::{net::SocketAddr, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing_futures::Instrument;

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
    let target_addr = SocketAddr::new(LOCALHOST.into(), 666);
    let local_addr = SocketAddr::new(LOCALHOST.into(), LISTEN_PORT);

    let cfg = default_config(target_addr);

    let (metrics, _) = Metrics::new(std::time::Duration::from_secs(10));
    let prevent_loop = super::PreventLoop::from(local_addr.port());

    // Configure mock IO for the upstream "server". It will read "hello" and
    // then write "world".
    let mut srv_io = io::mock();
    srv_io.write(b"hello").read(b"world");
    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = test_support::connect().endpoint_builder(target_addr, srv_io);
    let connect = cfg.build_tcp_connect_with(
        connect,
        tls::Conditional::None(tls::ReasonForNoPeerName::Loopback),
        &metrics.outbound,
    );
    // Configure mock IO for the "client".
    let client_io = io::mock().read(b"hello").write(b"world").build();

    // Configure the mock destination resolver to just give us a single endpoint
    // for the target, which always exists and has no metadata.
    let resolver = test_support::resolver().endpoint_exists(
        Addr::from(target_addr),
        target_addr,
        test_support::resolver::Metadata::empty(),
    );

    // Build the outbound TCP balancer stack.
    let outbound_tcp = cfg.build_tcp_balance(&connect, resolver, prevent_loop, &metrics.outbound);
    // Oneshot the TCP stack with the target and request (client conn).
    outbound_tcp.make_oneshot(target_addr, client_io).await
}

#[tokio::test(core_threads = 1)]
async fn tls_when_hinted() {
    let _trace = test_support::trace_init();

    let tls_addr = SocketAddr::new(LOCALHOST.into(), 5550);
    let plaintext_addr = SocketAddr::new(LOCALHOST.into(), 5551);
    let local_addr = SocketAddr::new(LOCALHOST.into(), LISTEN_PORT);

    let cfg = default_config(plaintext_addr);

    let (metrics, _) = Metrics::new(std::time::Duration::from_secs(10));
    let prevent_loop = super::PreventLoop::from(local_addr.port());

    let foo_id = test_support::identity("foo-ns1");
    let proxy_id = test_support::identity("bar-ns1")
        .local_identity(b"bar.ns1.serviceaccount.identity.linkerd.cluster.local");
    let id_name = linkerd2_identity::Name::from_hostname(
        b"foo.ns1.serviceaccount.identity.linkerd.cluster.local",
    )
    .expect("hostname is valid");
    let srv_cfg = foo_id.server_config.clone();

    // Configure mock IO for the upstream "server". It will read "hello" and
    // then write "world".
    let (d1, d2) = io::duplex(1024);
    let tls_srv = async move {
        let mut io = tokio_rustls::TlsAcceptor::from(srv_cfg)
            .accept(d2)
            .await
            .expect("failed to accept TLS handshake!");
        tracing::info!("handshake successful!");
        let mut s = vec![0; 5];
        io.read(&mut s).await.expect("read succeeds");
        tracing::info!(read = ?s);
        assert_eq!(&s[..], b"hello");
        io.write_all(b"world").await.expect("write succeeds");
        tracing::info!("wrote");
        io.shutdown().await.expect("close TLS connection");
        tracing::info!("shutdown");
    };

    let mut tls_srv_io = Some(d1);
    let mut srv_io = io::mock();
    srv_io.write(b"hello").read(b"world");

    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = test_support::connect()
        // The plaintext endpoint should use plaintext...
        .endpoint_builder(plaintext_addr, srv_io)
        .endpoint_fn(tls_addr, move || {
            tls_srv_io.take().expect("connected to TLS endpoint twice!")
        });
    let connect =
        cfg.build_tcp_connect_with(connect, tls::Conditional::Some(proxy_id), &metrics.outbound);

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
            Addr::from(plaintext_addr),
            plaintext_addr,
            test_support::resolver::Metadata::empty(),
        )
        .endpoint_exists(Addr::from(tls_addr), tls_addr, tls_meta);

    // Configure mock IO for the "client".
    let mut client_io = io::mock();
    client_io.read(b"hello").write(b"world");

    // Build the outbound TCP balancer stack.
    let outbound_tcp = cfg.build_tcp_balance(&connect, resolver, prevent_loop, &metrics.outbound);

    let io = client_io.build();
    let tls_conn = outbound_tcp
        .clone()
        .make_oneshot(tls_addr, io)
        .instrument(tracing::info_span!("tls_conn"));

    let io = client_io.build();
    let plain_conn = outbound_tcp
        .clone()
        .make_oneshot(plaintext_addr, io)
        .instrument(tracing::info_span!("plaintext_conn"));

    tokio::join! {
        tls_conn, plain_conn, tls_srv
    };
}

#[tokio::test(core_threads = 1)]
async fn server_first_tls_when_hinted() {
    let _trace = test_support::trace_init();

    let tls_addr = SocketAddr::new(LOCALHOST.into(), 5550);
    let plaintext_addr = SocketAddr::new(LOCALHOST.into(), 5551);
    let local_addr = SocketAddr::new(LOCALHOST.into(), LISTEN_PORT);

    let cfg = default_config(plaintext_addr);

    let (metrics, _) = Metrics::new(std::time::Duration::from_secs(10));
    let prevent_loop = super::PreventLoop::from(local_addr.port());

    let foo_id = test_support::identity("foo-ns1");
    let proxy_id = test_support::identity("bar-ns1")
        .local_identity(b"bar.ns1.serviceaccount.identity.linkerd.cluster.local");
    let id_name = linkerd2_identity::Name::from_hostname(
        b"foo.ns1.serviceaccount.identity.linkerd.cluster.local",
    )
    .expect("hostname is valid");
    let srv_cfg = foo_id.server_config.clone();

    // Configure mock IO for the upstream "server". It will read "hello" and
    // then write "world".
    let (d1, d2) = io::duplex(1024);
    let tls_srv = async move {
        let mut io = tokio_rustls::TlsAcceptor::from(srv_cfg)
            .accept(d2)
            .await
            .expect("failed to accept TLS handshake!");
        tracing::info!("handshake successful!");
        let mut s = vec![0; 5];
        io.write_all(b"hello").await.expect("write succeeds");
        tracing::info!("wrote");
        io.read(&mut s).await.expect("read succeeds");
        tracing::info!(read = ?s);
        assert_eq!(&s[..], b"world");
        io.shutdown().await.expect("close TLS connection");
        tracing::info!("shutdown");
    };
    let mut tls_srv_io = Some(d1);
    let mut srv_io = io::mock();
    srv_io.read(b"hello").write(b"world");

    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = test_support::connect()
        // The plaintext endpoint should use plaintext...
        .endpoint_builder(plaintext_addr, srv_io)
        .endpoint_fn(tls_addr, move || {
            tls_srv_io.take().expect("connected to TLS endpoint twice!")
        });
    let connect =
        cfg.build_tcp_connect_with(connect, tls::Conditional::Some(proxy_id), &metrics.outbound);

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
            Addr::from(plaintext_addr),
            plaintext_addr,
            test_support::resolver::Metadata::empty(),
        )
        .endpoint_exists(Addr::from(tls_addr), tls_addr, tls_meta);

    // Configure mock IO for the "client".
    let mut client_io = io::mock();
    client_io.write(b"hello").read(b"world");

    // Build the outbound TCP balancer stack.
    let outbound_tcp = cfg.build_tcp_balance(&connect, resolver, prevent_loop, &metrics.outbound);

    let io = client_io.build();
    let tls_conn = outbound_tcp
        .clone()
        .make_oneshot(tls_addr, io)
        .instrument(tracing::info_span!("tls_conn"));

    let io = client_io.build();
    let plain_conn = outbound_tcp
        .clone()
        .make_oneshot(plaintext_addr, io)
        .instrument(tracing::info_span!("plaintext_conn"));

    tokio::join! {
        tls_conn, plain_conn, tls_srv
    };
}
