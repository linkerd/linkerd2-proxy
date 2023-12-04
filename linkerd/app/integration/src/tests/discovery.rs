use crate::*;
use linkerd2_proxy_api as pb;

const HOST: &str = "disco.test.svc.cluster.local";

/// Cross-version test definitions.
///
/// These functions are not actual tests; instead, they are test *bodies* that
/// may be run with either an HTTP/1 or HTTP/2 server. The per-version test
/// modules contain tests that actually call these functions with the
/// appropriate version.
mod cross_version {
    use super::*;

    pub(super) struct Test {
        srv: server::Server,
        mk_client: fn(&proxy::Listening, &str) -> client::Client,
    }

    impl Test {
        pub(super) fn http2() -> Self {
            Self {
                srv: server::http2(),
                mk_client: |proxy, auth| client::http2(proxy.outbound, auth),
            }
        }

        pub(super) fn http1() -> Self {
            Self {
                srv: server::http1(),
                mk_client: |proxy, auth| client::http1(proxy.outbound, auth),
            }
        }

        pub(super) fn http1_absolute_uris() -> Self {
            Self {
                srv: server::http1(),
                mk_client: |proxy, auth| client::http1_absolute_uris(proxy.outbound, auth),
            }
        }
    }

    pub(super) async fn outbound_asks_controller_api(test: Test) {
        let _trace = trace_init();
        let srv = test
            .srv
            .route("/", "hello")
            .route("/bye", "bye")
            .run()
            .await;

        let dstctl = controller::new();
        let polctl = controller::policy();
        let _txs = send_default_dst(&dstctl, &polctl, &srv);

        let proxy = proxy::new()
            .controller(dstctl.run().await)
            .policy(polctl.run().await)
            .outbound(srv)
            .run()
            .await;
        let client = (test.mk_client)(&proxy, HOST);

        assert_eq!(client.get("/").await, "hello");
        assert_eq!(client.get("/bye").await, "bye");

        // Ensure panics are propagated.
        proxy.join_servers().await;
    }

    pub(super) async fn dst_resolutions_are_cached(test: Test) {
        // This test asserts that once a destination and profile have been
        // discovered for an original destination address, the same
        // discovery is reused by all connections with that original destination.
        let _trace = trace_init();
        let srv = test.srv.route("/", "hello").run().await;

        let dstctl = controller::new();
        let polctl = controller::policy();
        let (_profile, _policy, _dst) = send_default_dst(&dstctl, &polctl, &srv);
        dstctl.no_more_destinations();

        let proxy = proxy::new()
            .controller(dstctl.run().await)
            .policy(polctl.run().await)
            .outbound(srv)
            .run()
            .await;

        let client = (test.mk_client)(&proxy, HOST);
        assert_eq!(client.get("/").await, "hello");
        drop(client);

        let client = (test.mk_client)(&proxy, HOST);
        assert_eq!(client.get("/").await, "hello");
        drop(client);

        // Ensure panics are propagated.
        proxy.join_servers().await;
    }

    pub(super) async fn outbound_reconnects_if_controller_stream_ends(test: Test) {
        let _trace = trace_init();

        let srv = test.srv.route("/recon", "nect").run().await;

        let dstctl = controller::new();
        let polctl = controller::policy();
        let (_profile, _policy, dst) = send_default_dst(&dstctl, &polctl, &srv);

        drop(dst);

        let dst = dstctl.destination_tx(default_dst_name(&srv));
        dst.send_addr(srv.addr);

        let proxy = proxy::new()
            .controller(dstctl.run().await)
            .outbound(srv)
            .run()
            .await;
        let client = (test.mk_client)(&proxy, HOST);

        assert_eq!(client.get("/recon").await, "nect");
    }

    pub(super) async fn outbound_fails_fast_when_destination_has_no_endpoints(test: Test) {
        outbound_fails_fast(controller::destination_exists_with_no_endpoints(), test).await
    }

    pub(super) async fn outbound_fails_fast_when_destination_does_not_exist(test: Test) {
        outbound_fails_fast(controller::destination_does_not_exist(), test).await
    }

    async fn outbound_fails_fast(up: pb::destination::Update, test: Test) {
        use std::sync::atomic::{AtomicBool, Ordering};
        let _trace = trace_init();

        let did_not_fall_back = Arc::new(AtomicBool::new(true));
        let did_not_fall_back2 = did_not_fall_back.clone();

        let srv = test
            .srv
            .route_fn("/", move |_| {
                did_not_fall_back2.store(false, Ordering::Release);
                panic!()
            })
            .run()
            .await;

        let dstctl = controller::new();
        let polctl = controller::policy();
        let (_profile, _policy, dst) = send_default_dst(&dstctl, &polctl, &srv);

        dst.send(up);

        let proxy = proxy::new()
            .controller(dstctl.run().await)
            .policy(polctl.run().await)
            .outbound(srv)
            .run()
            .await;

        let client = (test.mk_client)(&proxy, HOST);

        let rsp = client.request(client.request_builder("/")).await.unwrap();

        assert!(
            did_not_fall_back.load(Ordering::Acquire),
            "original destination should not have been used!",
        );
        // We should have gotten an HTTP response, not an error.
        assert_eq!(rsp.status(), http::StatusCode::GATEWAY_TIMEOUT);

        // Ensure panics are propagated.
        proxy.join_servers().await;
    }

    pub(super) async fn outbound_falls_back_to_orig_dst_when_outside_search_path(test: Test) {
        let _trace = trace_init();

        let srv = test
            .srv
            .route("/", "hello from my great website")
            .run()
            .await;
        let mut env = TestEnv::default();
        // The test server will be on localhost, and we default to
        // configuring the profile search networks to include localhost so
        // that...every other test can work, so just put some random network
        // in there so it doesn't.
        env.put(
            app::env::ENV_DESTINATION_PROFILE_NETWORKS,
            "69.4.20.0/24".to_owned(),
        );

        let dstctl = controller::new();
        let polctl = controller::policy();

        dstctl.no_more_destinations();
        let proxy = proxy::new()
            .controller(dstctl.run().await)
            .policy(polctl.run().await)
            .outbound(srv)
            .run_with_test_env(env)
            .await;

        let client = proxy.outbound_http_client("my-great-websute.net");

        assert_eq!(client.get("/").await, "hello from my great website");

        // Ensure panics are propagated.
        proxy.join_servers().await;
    }

    pub(super) async fn outbound_falls_back_to_orig_dst_after_invalid_argument(test: Test) {
        let _trace = trace_init();

        let srv = test.srv.route("/", "hello").run().await;

        let dstctl = controller::new();
        let polctl = controller::policy();

        const NAME: &str = "unresolvable.svc.cluster.local";
        let profile = dstctl.profile_tx(srv.addr.to_string());
        profile.send_err(grpc::Status::new(
            grpc::Code::InvalidArgument,
            "unresolvable",
        ));
        dstctl.no_more_destinations();

        let proxy = proxy::new()
            .controller(dstctl.run().await)
            .policy(polctl.run().await)
            .outbound(srv)
            .run()
            .await;

        let client = (test.mk_client)(&proxy, NAME);

        assert_eq!(client.get("/").await, "hello");

        // Ensure panics are propagated.
        proxy.join_servers().await;
    }

    pub(super) async fn outbound_destinations_reset_on_reconnect_followed_by_empty(test: Test) {
        outbound_destinations_reset_on_reconnect(
            controller::destination_exists_with_no_endpoints(),
            test,
        )
        .await
    }

    pub(super) async fn outbound_destinations_reset_on_reconnect_followed_by_dne(test: Test) {
        outbound_destinations_reset_on_reconnect(controller::destination_does_not_exist(), test)
            .await
    }

    async fn outbound_destinations_reset_on_reconnect(up: pb::destination::Update, test: Test) {
        let _trace = trace_init();

        let srv = test.srv.route("/", "hello").run().await;

        let dstctl = controller::new();
        let _profile = dstctl.profile_tx_default(srv.addr, "initially-exists.ns.svc.cluster.local");

        let polctl = controller::policy().outbound_default(
            srv.addr,
            format!("initially-exists.ns.svc.cluster.local:{}", srv.addr.port()),
        );

        let dst_tx0 = dstctl.destination_tx(format!(
            "initially-exists.ns.svc.cluster.local:{}",
            srv.addr.port()
        ));
        dst_tx0.send_addr(srv.addr);

        let dst_tx1 = dstctl.destination_tx(format!(
            "initially-exists.ns.svc.cluster.local:{}",
            srv.addr.port()
        ));

        let proxy = proxy::new()
            .controller(dstctl.run().await)
            .policy(polctl.run().await)
            .outbound(srv)
            .run()
            .await;

        let initially_exists = (test.mk_client)(&proxy, "initially-exists.ns.svc.cluster.local");
        assert_eq!(initially_exists.get("/").await, "hello");

        drop(dst_tx0); // trigger reconnect
        dst_tx1.send(up);

        // Wait for the reconnect to happen. TODO: Replace this flaky logic.
        tokio::time::sleep(Duration::from_millis(1000)).await;

        let rsp = initially_exists
            .request(initially_exists.request_builder("/"))
            .await
            .unwrap();
        assert_eq!(rsp.status(), http::StatusCode::GATEWAY_TIMEOUT);

        // Ensure panics are propagated.
        proxy.join_servers().await;
    }

    pub(super) async fn outbound_times_out(test: Test) {
        let _t = trace_init();

        let srv = test.srv.route("/hi", "hello").run().await;

        let dstctl = controller::new();
        let _profile = dstctl.profile_tx_default(srv.addr, HOST);

        let polctl = controller::policy().outbound_default(srv.addr, default_dst_name(&srv));

        // when the proxy requests the destination, don't respond.
        let _dst_tx = dstctl.destination_tx(default_dst_name(&srv));
        let _txs = send_default_dst(&dstctl, &polctl, &srv);

        let proxy = proxy::new()
            .controller(dstctl.run().await)
            .policy(polctl.run().await)
            .outbound(srv)
            .run()
            .await;

        let client = (test.mk_client)(&proxy, HOST);
        let req = client.request_builder("/");
        let rsp = client.request(req.method("GET")).await.unwrap();
        // The request should time out.
        assert_eq!(rsp.status(), http::StatusCode::GATEWAY_TIMEOUT);

        // Ensure panics are propagated.
        proxy.join_servers().await;
    }

    pub(super) async fn outbound_error_reconnects_after_backoff(test: Test) {
        let _trace = trace_init();

        let srv = test.srv.route("/", "hello").run().await;

        // Used to delay `listen` in the server, to force connection refused errors.
        let (tx, rx) = oneshot::channel::<()>();

        let dstctl = controller::new();
        let polctl = controller::policy();
        let _txs = send_default_dst(&dstctl, &polctl, &srv);

        let proxy = proxy::new()
            .controller(
                dstctl
                    .delay_listen(async move {
                        let _ = rx.await;
                    })
                    .await,
            )
            .outbound_ip(srv.addr)
            .run()
            .await;

        // Allow the control client to notice a connection error
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Allow our controller to start accepting connections,
        // and then wait a little bit so the client tries again.
        drop(tx);
        tokio::time::sleep(Duration::from_millis(500)).await;

        let client = (test.mk_client)(&proxy, HOST);

        assert_eq!(client.get("/").await, "hello");

        // Ensure panics are propagated.
        srv.join().await;
    }
}

fn default_dst_name(srv: &server::Listening) -> String {
    format!("{}:{}", HOST, srv.addr.port())
}

fn send_default_dst(
    dstctl: &controller::Controller,
    polctl: &policy::Controller,
    srv: &server::Listening,
) -> (
    controller::ProfileSender,
    policy::OutboundSender,
    controller::DstSender,
) {
    let addr = srv.addr;
    let name = default_dst_name(srv);
    tracing::info!("Configuring resolution for {addr} {name}");

    let policy = polctl.outbound_tx_default(addr, name.clone());
    let profile = dstctl.profile_tx_default(addr, HOST);

    let dst = dstctl.destination_tx(name);
    dst.send_addr(addr);

    (profile, policy, dst)
}

mod http2 {
    use super::*;

    version_tests! {
        cross_version::Test::http2() =>
        outbound_asks_controller_api,
        dst_resolutions_are_cached,
        outbound_reconnects_if_controller_stream_ends,
        outbound_fails_fast_when_destination_has_no_endpoints,
        outbound_fails_fast_when_destination_does_not_exist,
        outbound_falls_back_to_orig_dst_when_outside_search_path,
        outbound_falls_back_to_orig_dst_after_invalid_argument,
        outbound_destinations_reset_on_reconnect_followed_by_empty,
        outbound_destinations_reset_on_reconnect_followed_by_dne,
        outbound_times_out,
        outbound_error_reconnects_after_backoff
    }

    #[tokio::test]
    async fn outbound_balancer_waits_for_ready_endpoint() {
        // See https://github.com/linkerd/linkerd2/issues/2550
        let _t = trace_init();

        let srv1 = server::http2()
            .route("/", "hello")
            .route("/bye", "bye")
            .run()
            .await;

        let srv2 = server::http2()
            .route("/", "hello")
            .route("/bye", "bye")
            .run()
            .await;

        // Start with the first server.
        let dstctl = controller::new();
        let polctl = controller::policy();
        let (_profile, _policy, dst) = send_default_dst(&dstctl, &polctl, &srv1);

        let proxy = proxy::new()
            .outbound_ip(srv1.addr)
            .controller(dstctl.run().await)
            .policy(polctl.run().await)
            .run()
            .await;
        let client = client::http2(proxy.outbound, HOST);
        let metrics = client::http1(proxy.admin, "localhost");

        assert_eq!(client.get("/").await, "hello");

        // Simulate the first server falling over without discovery
        // knowing about it...
        srv1.join().await;
        tokio::task::yield_now().await;

        // Wait until the proxy has seen the `srv1` disconnect...
        metrics::metric("tcp_close_total")
            .label("peer", "dst")
            .label("direction", "outbound")
            .value(1u64)
            .assert_in(&metrics)
            .await;

        // Start a new request to the destination, now that the server is dead.
        // This request should be waiting at the balancer for a ready endpoint.
        //
        // The only one it knows about is dead, so it won't have progressed.
        let fut = client.request(client.request_builder("/bye"));

        // When we tell the balancer about a new endpoint, it should have added
        // it and then dispatched the request...
        dst.send_addr(srv2.addr);

        let res = fut.await.expect("/bye response");
        assert_eq!(res.status(), http::StatusCode::OK);
    }
}

mod http1 {
    use super::*;

    version_tests! {
        cross_version::Test::http1() =>
        outbound_asks_controller_api,
        dst_resolutions_are_cached,
        outbound_reconnects_if_controller_stream_ends,
        outbound_fails_fast_when_destination_has_no_endpoints,
        outbound_fails_fast_when_destination_does_not_exist,
        outbound_falls_back_to_orig_dst_when_outside_search_path,
        outbound_falls_back_to_orig_dst_after_invalid_argument,
        outbound_destinations_reset_on_reconnect_followed_by_empty,
        outbound_destinations_reset_on_reconnect_followed_by_dne,
        outbound_times_out,
        outbound_error_reconnects_after_backoff
    }

    mod absolute_uris {
        use super::*;

        version_tests! {
            cross_version::Test::http1_absolute_uris() =>
            outbound_asks_controller_api,
            dst_resolutions_are_cached,
            outbound_reconnects_if_controller_stream_ends,
            outbound_fails_fast_when_destination_has_no_endpoints,
            outbound_fails_fast_when_destination_does_not_exist,
            outbound_falls_back_to_orig_dst_when_outside_search_path,
            outbound_falls_back_to_orig_dst_after_invalid_argument,
            outbound_destinations_reset_on_reconnect_followed_by_empty,
            outbound_destinations_reset_on_reconnect_followed_by_dne,
            outbound_times_out,
            outbound_error_reconnects_after_backoff
        }
    }
}
