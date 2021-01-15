macro_rules! generate_tests {
    (server: $make_server:path, client: $make_client:path) => {
        use linkerd2_proxy_api as pb;

        #[tokio::test]
        async fn outbound_asks_controller_api() {
            let _trace = trace_init();
            let srv = $make_server()
                .route("/", "hello")
                .route("/bye", "bye")
                .run()
                .await;

            let ctrl = controller::new();
            let _profile = ctrl.profile_tx_default(srv.addr, "disco.test.svc.cluster.local");
            let dest =
                ctrl.destination_tx(format!("disco.test.svc.cluster.local:{}", srv.addr.port()));
            dest.send_addr(srv.addr);

            let proxy = proxy::new()
                .controller(ctrl.run().await)
                .outbound(srv)
                .run()
                .await;
            let client = $make_client(proxy.outbound, "disco.test.svc.cluster.local");

            assert_eq!(client.get("/").await, "hello");
            assert_eq!(client.get("/bye").await, "bye");

            // Ensure panics are propagated.
            proxy.join_servers().await;
        }

        #[tokio::test]
        async fn dst_resolutions_are_cached() {
            // This test asserts that once a destination and profile have been
            // discovered for an original destination address, the same
            // discovery is reused by all connections with that original destination.
            let _trace = trace_init();
            let srv = $make_server().route("/", "hello").run().await;

            let ctrl = controller::new();
            let _profile = ctrl.profile_tx_default(srv.addr, "disco.test.svc.cluster.local");
            let dest =
                ctrl.destination_tx(format!("disco.test.svc.cluster.local:{}", srv.addr.port()));
            dest.send_addr(srv.addr);
            ctrl.no_more_destinations();

            let proxy = proxy::new()
                .controller(ctrl.run().await)
                .outbound(srv)
                .run()
                .await;

            let client = $make_client(proxy.outbound, "disco.test.svc.cluster.local");

            assert_eq!(client.get("/").await, "hello");
            drop(client);

            let client = $make_client(proxy.outbound, "disco.test.svc.cluster.local");
            assert_eq!(client.get("/").await, "hello");
            drop(client);

            // Ensure panics are propagated.
            proxy.join_servers().await;
        }

        #[tokio::test]
        async fn outbound_reconnects_if_controller_stream_ends() {
            let _trace = trace_init();

            let srv = $make_server().route("/recon", "nect").run().await;

            let ctrl = controller::new();
            let _profile = ctrl.profile_tx_default(srv.addr, "disco.test.svc.cluster.local");
            drop(ctrl.destination_tx(format!("disco.test.svc.cluster.local:{}", srv.addr.port())));
            let dest =
                ctrl.destination_tx(format!("disco.test.svc.cluster.local:{}", srv.addr.port()));
            dest.send_addr(srv.addr);

            let proxy = proxy::new()
                .controller(ctrl.run().await)
                .outbound(srv)
                .run()
                .await;
            let client = $make_client(proxy.outbound, "disco.test.svc.cluster.local");

            assert_eq!(client.get("/recon").await, "nect");
        }

        #[tokio::test]
        async fn outbound_fails_fast_when_destination_has_no_endpoints() {
            outbound_fails_fast(controller::destination_exists_with_no_endpoints()).await
        }

        #[tokio::test]
        async fn outbound_fails_fast_when_destination_does_not_exist() {
            outbound_fails_fast(controller::destination_does_not_exist()).await
        }

        async fn outbound_fails_fast(up: pb::destination::Update) {
            use std::sync::{
                atomic::{AtomicBool, Ordering},
                Arc,
            };
            let _trace = trace_init();

            let did_not_fall_back = Arc::new(AtomicBool::new(true));
            let did_not_fall_back2 = did_not_fall_back.clone();

            let srv = $make_server()
                .route_fn("/", move |_| {
                    did_not_fall_back2.store(false, Ordering::Release);
                    panic!()
                })
                .run()
                .await;

            let ctrl = controller::new();
            let _profile = ctrl.profile_tx_default(srv.addr, "disco.test.svc.cluster.local");
            let dest =
                ctrl.destination_tx(format!("disco.test.svc.cluster.local:{}", srv.addr.port()));
            dest.send(up);

            let proxy = proxy::new()
                .controller(ctrl.run().await)
                .outbound(srv)
                .run()
                .await;

            let client = $make_client(proxy.outbound, "disco.test.svc.cluster.local");

            let rsp = client.request(client.request_builder("/")).await.unwrap();

            assert!(
                did_not_fall_back.load(Ordering::Acquire),
                "original destination should not have been used!",
            );
            // We should have gotten an HTTP response, not an error.
            assert_eq!(rsp.status(), http::StatusCode::SERVICE_UNAVAILABLE);

            // Ensure panics are propagated.
            proxy.join_servers().await;
        }

        #[tokio::test]
        async fn outbound_falls_back_to_orig_dst_when_outside_search_path() {
            let _trace = trace_init();

            let srv = $make_server()
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
            let ctrl = controller::new();
            ctrl.no_more_destinations();
            let proxy = proxy::new()
                .controller(ctrl.run().await)
                .outbound(srv)
                .run_with_test_env(env)
                .await;

            let client = $make_client(proxy.outbound, "my-great-websute.net");

            assert_eq!(client.get("/").await, "hello from my great website");

            // Ensure panics are propagated.
            proxy.join_servers().await;
        }

        #[tokio::test]
        async fn outbound_falls_back_to_orig_dst_after_invalid_argument() {
            let _trace = trace_init();

            let srv = $make_server().route("/", "hello").run().await;

            const NAME: &str = "unresolvable.svc.cluster.local";
            let ctrl = controller::new();
            let profile = ctrl.profile_tx(&srv.addr.to_string());
            profile.send_err(grpc::Status::new(
                grpc::Code::InvalidArgument,
                "unresolvable",
            ));
            ctrl.no_more_destinations();

            let proxy = proxy::new()
                .controller(ctrl.run().await)
                .outbound(srv)
                .run()
                .await;

            let client = $make_client(proxy.outbound, NAME);

            assert_eq!(client.get("/").await, "hello");

            // Ensure panics are propagated.
            proxy.join_servers().await;
        }

        #[tokio::test]
        async fn outbound_destinations_reset_on_reconnect_followed_by_empty() {
            outbound_destinations_reset_on_reconnect(
                controller::destination_exists_with_no_endpoints(),
            )
            .await
        }

        #[tokio::test(flavor = "current_thread")]
        async fn outbound_destinations_reset_on_reconnect_followed_by_dne() {
            outbound_destinations_reset_on_reconnect(controller::destination_does_not_exist()).await
        }

        async fn outbound_destinations_reset_on_reconnect(up: pb::destination::Update) {
            let _trace = trace_init();
            let env = TestEnv::default();
            let srv = $make_server().route("/", "hello").run().await;
            let ctrl = controller::new();
            let _profile =
                ctrl.profile_tx_default(srv.addr, "initially-exists.ns.svc.cluster.local");

            let dst_tx0 = ctrl.destination_tx(format!(
                "initially-exists.ns.svc.cluster.local:{}",
                srv.addr.port()
            ));
            dst_tx0.send_addr(srv.addr);

            let dst_tx1 = ctrl.destination_tx(format!(
                "initially-exists.ns.svc.cluster.local:{}",
                srv.addr.port()
            ));

            let proxy = proxy::new()
                .controller(ctrl.run().await)
                .outbound(srv)
                .run_with_test_env(env)
                .await;

            let initially_exists =
                $make_client(proxy.outbound, "initially-exists.ns.svc.cluster.local");
            assert_eq!(initially_exists.get("/").await, "hello");

            drop(dst_tx0); // trigger reconnect
            dst_tx1.send(up);

            // Wait for the reconnect to happen. TODO: Replace this flaky logic.
            tokio::time::sleep(Duration::from_millis(1000)).await;

            let rsp = initially_exists
                .request(initially_exists.request_builder("/"))
                .await
                .unwrap();
            assert_eq!(rsp.status(), http::StatusCode::SERVICE_UNAVAILABLE);

            // Ensure panics are propagated.
            proxy.join_servers().await;
        }

        #[tokio::test]
        async fn outbound_times_out() {
            let env = TestEnv::default();

            let srv = $make_server().route("/hi", "hello").run().await;
            let ctrl = controller::new();

            let _profile = ctrl.profile_tx_default(srv.addr, "disco.test.svc.cluster.local");

            // when the proxy requests the destination, don't respond.
            let _dst_tx =
                ctrl.destination_tx(format!("disco.test.svc.cluster.local:{}", srv.addr.port()));

            let proxy = proxy::new()
                .controller(ctrl.run().await)
                .outbound(srv)
                .run_with_test_env(env)
                .await;

            let client = $make_client(proxy.outbound, "disco.test.svc.cluster.local");
            let req = client.request_builder("/");
            let rsp = client.request(req.method("GET")).await.unwrap();
            // the request should time out
            assert_eq!(rsp.status(), http::StatusCode::SERVICE_UNAVAILABLE);

            // Ensure panics are propagated.
            proxy.join_servers().await;
        }

        #[tokio::test]
        async fn outbound_error_reconnects_after_backoff() {
            let _trace = trace_init();

            let srv = $make_server().route("/", "hello").run().await;

            // Used to delay `listen` in the server, to force connection refused errors.
            let (tx, rx) = oneshot::channel::<()>();

            let ctrl = controller::new();
            let _profile = ctrl.profile_tx_default(srv.addr, "disco.test.svc.cluster.local");

            let dst_tx =
                ctrl.destination_tx(format!("disco.test.svc.cluster.local:{}", srv.addr.port()));
            dst_tx.send_addr(srv.addr);
            // but don't drop, to not trigger stream closing reconnects

            let proxy = proxy::new()
                .controller(
                    ctrl.delay_listen(async move {
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

            let client = $make_client(proxy.outbound, "disco.test.svc.cluster.local");

            assert_eq!(client.get("/").await, "hello");

            // Ensure panics are propagated.
            srv.join().await;
        }
    };
}

mod http2 {
    use crate::*;

    generate_tests! { server: server::new, client: client::new }

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
        let host = "disco.test.svc.cluster.local";
        let port = srv1.addr.port();
        let ctrl = controller::new();
        let _profile = ctrl.profile_tx_default(srv1.addr, host);
        let dst = ctrl.destination_tx(&format!("{}:{}", host, port));
        // Start by "knowing" the first server...
        dst.send_addr(srv1.addr);

        let proxy = proxy::new()
            .outbound_ip(srv1.addr)
            .controller(ctrl.run().await)
            .run()
            .await;
        let client = client::http2(proxy.outbound, host);
        let metrics = client::http1(proxy.metrics, "localhost");

        assert_eq!(client.get("/").await, "hello");

        // Simulate the first server falling over without discovery
        // knowing about it...
        srv1.join().await;
        tokio::task::yield_now().await;

        // Wait until the proxy has seen the `srv1` disconnect...
        let metric = metrics::metric("tcp_close_total")
            .with_label("peer", "dst")
            .with_label("direction", "outbound")
            .with_label("tls", "no_identity")
            .with_label("no_tls_reason", "not_provided_by_service_discovery")
            .with_label(
                "authority",
                format_args!("disco.test.svc.cluster.local:{}", port),
            )
            .with_value(1);
        assert_eventually_contains!(metrics.get("/metrics").await, metric);

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
    use crate::*;

    generate_tests! {
        server: server::http1, client: client::http1
    }

    mod absolute_uris {
        use crate::*;

        generate_tests! {
            server: server::http1,
            client: client::http1_absolute_uris
        }
    }
}
