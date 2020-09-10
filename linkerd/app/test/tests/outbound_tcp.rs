use linkerd2_app_test::*;
use tower::ServiceExt;

#[tokio::test]
async fn it_basically_works() {
    let _g = trace_init();
    let mut env = TestEnv::new();
    // We don't actually need any of this except that it's currently the only
    // way to build an `outbound::Config`...we should fix that later.
    env.put(
        "LINKERD2_PROXY_DESTINATION_SVC_ADDR",
        "127.0.0.1:9999".to_owned(),
    );
    env.put(app::env::ENV_OUTBOUND_LISTEN_ADDR, "127.0.0.1:0".to_owned());
    env.put(
        app::env::ENV_INBOUND_ORIG_DST_ADDR,
        "127.0.0.1:0".to_owned(),
    );
    env.put(
        app::env::ENV_OUTBOUND_ORIG_DST_ADDR,
        "127.0.0.1:0".to_owned(),
    );
    env.put(app::env::ENV_INBOUND_LISTEN_ADDR, "127.0.0.1:0".to_owned());
    env.put(app::env::ENV_CONTROL_LISTEN_ADDR, "127.0.0.1:0".to_owned());
    env.put(app::env::ENV_ADMIN_LISTEN_ADDR, "127.0.0.1:0".to_owned());
    env.put("LINKERD2_PROXY_IDENTITY_DISABLED", "true".to_owned());
    env.put("LINKERD2_PROXY_TAP_DISABLED", "true".to_owned());

    let config = app::env::parse_config(&env).unwrap();
    let (metrics, _) = linkerd2_app::Metrics::new(config.admin.metrics_retain_idle);
    let target_addr = "127.0.0.1:666".parse::<SocketAddr>().unwrap();
    let local_addr = "127.0.0.1:4140".parse::<SocketAddr>().unwrap();
    let peer_addr = "127.0.0.1:420".parse::<SocketAddr>().unwrap();
    let prevent_loop = linkerd2_app_outbound::PreventLoop::from(local_addr.port());

    // Configure mock IO for the upstream "server". It will read "hello" and
    // then write "world".
    let mut srv_io = mock::io();
    srv_io.read(b"hello").write(b"world");
    let connect = mock::connect().endpoint_builder(target_addr, srv_io);

    // Configure mock IO for the "client".
    let client_io = mock::io().write(b"hello").read(b"world").build();

    // Configure the mock destination resolver to just give us a single endpoint
    // for the target, which always exists and has no metadata.
    let resolver = mock::resolver().endpoint_exists(
        Addr::from(target_addr),
        target_addr,
        mock::resolver::Metadata::empty(),
    );

    let outbound_tcp =
        config
            .outbound
            .build_tcp_balance(&connect, resolver, prevent_loop, &metrics.outbound);
    let svc = outbound_tcp
        .oneshot(linkerd2_proxy_transport::listen::Addrs::new(
            local_addr,
            peer_addr,
            Some(target_addr),
        ))
        .await
        .expect("make service should succeed");
    svc.oneshot(client_io).await.expect("conn should succeed")
}
