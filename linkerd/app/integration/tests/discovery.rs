#![deny(warnings, rust_2018_idioms)]
#![type_length_limit = "16289823"]
#![recursion_limit = "256"]

use linkerd2_app_integration::*;

macro_rules! generate_tests {
    (server: $make_server:path, client: $make_client:path) => {
        use linkerd2_proxy_api as pb;

        #[tokio::test]
        async fn outbound_asks_controller_api() {
            let _trace = trace_init();
            let srv = $make_server().route("/", "hello").route("/bye", "bye").run().await;

            let ctrl = controller::new();
            ctrl.profile_tx_default("disco.test.svc.cluster.local");
            ctrl.destination_tx("disco.test.svc.cluster.local").send_addr(srv.addr);

            let proxy = proxy::new().controller(ctrl.run().await).outbound(srv).run().await;
            let client = $make_client(proxy.outbound, "disco.test.svc.cluster.local");

            assert_eq!(client.get("/").await, "hello");
            assert_eq!(client.get("/bye").await, "bye");

            // Ensure panics are propagated.
            proxy.join_servers().await;
        }

        #[tokio::test]
        async fn outbound_reconnects_if_controller_stream_ends() {
            let _trace = trace_init();

            let srv = $make_server().route("/recon", "nect").run().await;

            let ctrl = controller::new();
            ctrl.profile_tx_default("disco.test.svc.cluster.local");
            drop(ctrl.destination_tx("disco.test.svc.cluster.local"));
            ctrl.destination_tx("disco.test.svc.cluster.local").send_addr(srv.addr);

            let proxy = proxy::new().controller(ctrl.run().await).outbound(srv).run().await;
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
            use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
            let _trace = trace_init();

            let did_not_fall_back = Arc::new(AtomicBool::new(true));
            let did_not_fall_back2 = did_not_fall_back.clone();

            let srv = $make_server().route_fn("/", move |_| {
                did_not_fall_back2.store(false, Ordering::Release);
                panic!()
            }).run().await;

            let ctrl = controller::new();
            ctrl.profile_tx_default("disco.test.svc.cluster.local");
            ctrl.destination_tx("disco.test.svc.cluster.local").send(up);

            let proxy = proxy::new()
                .controller(ctrl.run().await)
                .outbound(srv)
                .run().await;

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

            let srv = $make_server().route("/", "hello from my great website").run().await;

            let ctrl = controller::new();
            ctrl.no_more_destinations();
            let proxy = proxy::new()
                .controller(ctrl.run().await)
                .outbound(srv)
                .run().await;

            let client = $make_client(proxy.outbound, "my-great-websute.net");

            assert_eq!(client.get("/").await, "hello from my great website");

            // Ensure panics are propagated.
            proxy.join_servers().await;
        }

        #[tokio::test]
        async fn outbound_falls_back_to_orig_dst_after_invalid_argument() {
            let _trace = trace_init();

            let srv = $make_server().route("/", "hello").run().await;

            const NAME: &'static str = "unresolvable.svc.cluster.local";
            let ctrl = controller::new();
            ctrl.profile_tx_default(NAME);
            ctrl.destination_fail(
                NAME,
                grpc::Status::new(grpc::Code::InvalidArgument, "unresolvable"),
            );
            ctrl.no_more_destinations();

            let proxy = proxy::new()
                .controller(ctrl.run().await)
                .outbound(srv)
                .run().await;

            let client = $make_client(proxy.outbound, NAME);

            assert_eq!(client.get("/").await, "hello");

            // Ensure panics are propagated.
            proxy.join_servers().await;
        }

        #[tokio::test]
        async fn outbound_destinations_reset_on_reconnect_followed_by_empty() {
            outbound_destinations_reset_on_reconnect(
                controller::destination_exists_with_no_endpoints()
            ).await
        }

        #[tokio::test]
        async fn outbound_destinations_reset_on_reconnect_followed_by_dne() {
            outbound_destinations_reset_on_reconnect(
                controller::destination_does_not_exist()
            ).await
        }

        async fn outbound_destinations_reset_on_reconnect(up: pb::destination::Update) {

            let env = TestEnv::new();
            let srv = $make_server().route("/", "hello").run().await;
            let ctrl = controller::new();
            ctrl.profile_tx_default("initially-exists.ns.svc.cluster.local");

            let dst_tx0 = ctrl.destination_tx("initially-exists.ns.svc.cluster.local");
            dst_tx0.send_addr(srv.addr);

            let dst_tx1 = ctrl.destination_tx("initially-exists.ns.svc.cluster.local");

            let proxy = proxy::new()
                .controller(ctrl.run().await)
                .outbound(srv)
                .run_with_test_env(env).await;

            let initially_exists =
                $make_client(proxy.outbound, "initially-exists.ns.svc.cluster.local");
            assert_eq!(initially_exists.get("/").await, "hello");

            drop(dst_tx0); // trigger reconnect
            dst_tx1.send(up);

            // Wait for the reconnect to happen. TODO: Replace this flaky logic.
            tokio::time::delay_for(Duration::from_millis(1000)).await;

            let rsp = initially_exists.request(initially_exists.request_builder("/")).await.unwrap();
            assert_eq!(rsp.status(), http::StatusCode::SERVICE_UNAVAILABLE);

            // Ensure panics are propagated.
            proxy.join_servers().await;
        }

        #[tokio::test]
        #[ignore] //TODO: there's currently no destination-acquisition timeout...
        async fn outbound_times_out() {
            let env = TestEnv::new();

            let srv = $make_server().route("/hi", "hello").run().await;
            let ctrl = controller::new();

            ctrl.profile_tx_default("disco.test.svc.cluster.local");

            // when the proxy requests the destination, don't respond.
            let _dst_tx = ctrl
                .destination_tx("disco.test.svc.cluster.local");

            let proxy = proxy::new()
                .controller(ctrl.run().await)
                .outbound(srv)
                .run_with_test_env(env)
                .await;

            let client = $make_client(proxy.outbound, "disco.test.svc.cluster.local");
            let req = client.request_builder("/");
            let rsp = client.request(req.method("GET")).await.unwrap();
            // the request should time out
            assert_eq!(rsp.status(), http::StatusCode::INTERNAL_SERVER_ERROR);

            // Ensure panics are propagated.
            proxy.join_servers().await;
        }

        #[tokio::test]
        async fn outbound_asks_controller_without_orig_dst() {
            let _trace = trace_init();
            let _ = TestEnv::new();

            let srv = $make_server()
                .route("/", "hello")
                .route("/bye", "bye")
                .run()
                .await;

            let ctrl = controller::new();
            ctrl.profile_tx_default("disco.test.svc.cluster.local");
            ctrl.destination_tx("disco.test.svc.cluster.local").send_addr(srv.addr);

            let proxy = proxy::new()
                .controller(ctrl.run().await)
                // don't set srv as outbound(), so that SO_ORIGINAL_DST isn't
                // used as a backup
                .run().await;
            let client = $make_client(proxy.outbound, "disco.test.svc.cluster.local");

            assert_eq!(client.get("/").await, "hello");
            assert_eq!(client.get("/bye").await, "bye");

            // Ensure panics are propagated.
            srv.join().await;
        }

        #[tokio::test]
        async fn outbound_error_reconnects_after_backoff() {
            let env = TestEnv::new();

            let srv = $make_server()
                .route("/", "hello")
                .run().await;

            // Used to delay `listen` in the server, to force connection refused errors.
            let (tx, rx) = oneshot::channel::<()>();

            let ctrl = controller::new();
            ctrl.profile_tx_default("disco.test.svc.cluster.local");

            let dst_tx = ctrl.destination_tx("disco.test.svc.cluster.local");
            dst_tx.send_addr(srv.addr);
            // but don't drop, to not trigger stream closing reconnects

            let proxy = proxy::new()
                .controller(ctrl.delay_listen(async move { let _ = rx.await; }).await)
                // don't set srv as outbound(), so that SO_ORIGINAL_DST isn't
                // used as a backup
                .run_with_test_env(env).await;

            // Allow the control client to notice a connection error
            tokio::time::delay_for(Duration::from_millis(500)).await;

            // Allow our controller to start accepting connections,
            // and then wait a little bit so the client tries again.
            drop(tx);
            tokio::time::delay_for(Duration::from_millis(500)).await;

            let client = $make_client(proxy.outbound, "disco.test.svc.cluster.local");

            assert_eq!(client.get("/").await, "hello");

            // Ensure panics are propagated.
            srv.join().await;
        }

        mod remote_header {
            use http::header::HeaderValue;
            use super::super::*;

            const REMOTE_IP_HEADER: &'static str = "l5d-remote-ip";
            const IP_1: &'static str = "0.0.0.0";
            const IP_2: &'static str = "127.0.0.1";

            #[tokio::test]
            async fn outbound_should_strip() {
                let _trace = trace_init();
                let header = HeaderValue::from_static(IP_1);

                let srv = $make_server().route_fn("/strip", |_req| {
                    Response::builder().header(REMOTE_IP_HEADER, IP_1).body(Default::default()).unwrap()
                }).run().await;

                let ctrl = controller::new();
                ctrl.profile_tx_default("disco.test.svc.cluster.local");
                ctrl.destination_tx("disco.test.svc.cluster.local").send_addr(srv.addr);
                let proxy = proxy::new().controller(ctrl.run().await).outbound(srv).run().await;
                let client = $make_client(proxy.outbound, "disco.test.svc.cluster.local");
                let rsp = client.request(client.request_builder("/strip")).await.unwrap();

                assert_eq!(rsp.status(), 200);
                assert_ne!(rsp.headers().get(REMOTE_IP_HEADER), Some(&header));
            }

            #[tokio::test]
            async fn inbound_should_strip() {
                let _trace = trace_init();
                let header = HeaderValue::from_static(IP_1);

                let srv = $make_server().route_fn("/strip", move |req| {
                    assert_ne!(req.headers().get(REMOTE_IP_HEADER), Some(&header));
                    Response::default()
                }).run().await;

                let ctrl = controller::new();
                ctrl.profile_tx_default("disco.test.svc.cluster.local");
                let proxy = proxy::new().controller(ctrl.run().await).inbound(srv).run().await;
                let client = $make_client(proxy.inbound, "disco.test.svc.cluster.local");
                let rsp = client.request(client.request_builder("/strip").header(REMOTE_IP_HEADER, IP_1)).await.unwrap();

                assert_eq!(rsp.status(), 200);

                // Ensure panics are propagated.
                proxy.join_servers().await;
            }

            #[tokio::test]
            #[ignore] // #2597
            async fn outbound_should_set() {
                let _trace = trace_init();
                let header = HeaderValue::from_static(IP_2);

                let srv = $make_server().route("/set", "hello").run().await;
                let ctrl = controller::new();
                ctrl.profile_tx_default("disco.test.svc.cluster.local");
                ctrl.destination_tx("disco.test.svc.cluster.local").send_addr(srv.addr);
                let proxy = proxy::new().controller(ctrl.run().await).outbound(srv).run().await;
                let client = $make_client(proxy.outbound, "disco.test.svc.cluster.local");
                let rsp = client.request(client.request_builder("/set")).await.unwrap();

                assert_eq!(rsp.status(), 200);
                assert_eq!(rsp.headers().get(REMOTE_IP_HEADER), Some(&header));
            }

            #[tokio::test]
            #[ignore] // #2597
            async fn inbound_should_set() {
                let _trace = trace_init();

                let header = HeaderValue::from_static(IP_2);

                let srv = $make_server().route_fn("/set", move |req| {
                    assert_eq!(req.headers().get(REMOTE_IP_HEADER), Some(&header));
                    Response::default()
                }).run().await;

                let proxy = proxy::new().inbound(srv).run().await;
                let client = $make_client(proxy.inbound, "disco.test.svc.cluster.local");
                let rsp = client.request(client.request_builder("/set")).await.unwrap();

                assert_eq!(rsp.status(), 200);

                // Ensure panics are propagated.
                proxy.join_servers().await;
            }
        }

        mod override_header {
            use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
            use bytes::buf::Buf;
            use super::super::*;

            const OVERRIDE_HEADER: &'static str = "l5d-dst-override";
            const FOO: &'static str = "foo.test.svc.cluster.local";
            const BAR: &'static str = "bar.test.svc.cluster.local";

            struct Fixture {
                foo_reqs: Arc<AtomicUsize>,
                bar_reqs: Arc<AtomicUsize>,
                foo: Option<server::Listening>,
                bar: server::Listening,
                ctrl: Option<controller::Controller>,
                _foo_dst: (controller::ProfileSender, controller::DstSender),
                _bar_dst: (controller::ProfileSender, controller::DstSender),
            }

            impl Fixture {
                async fn new() -> Fixture {
                    let _trace = trace_init();

                    let foo_reqs = Arc::new(AtomicUsize::new(0));
                    let foo_reqs2 = foo_reqs.clone();
                    let foo = $make_server()
                        .route_fn("/", move |req| {
                            assert!(
                                !req.headers().contains_key(OVERRIDE_HEADER),
                                "dst override header should be stripped before forwarding request",
                            );
                            foo_reqs2.clone().fetch_add(1, Ordering::Release);
                            Response::builder().status(200)
                                .body(Bytes::from_static(&b"hello from foo"[..]))
                                .unwrap()
                        })
                        .run().await;

                    let bar_reqs = Arc::new(AtomicUsize::new(0));
                    let bar_reqs2 = bar_reqs.clone();
                    let bar = $make_server()
                        .route_fn("/", move |req| {
                            assert!(
                                !req.headers().contains_key(OVERRIDE_HEADER),
                                "dst override header should be stripped before forwarding request",
                            );
                            bar_reqs2.clone().fetch_add(1, Ordering::Release);
                            Response::builder().status(200)
                                .body(Bytes::from_static(&b"hello from bar"[..]))
                                .unwrap()
                        })
                        .run().await;

                    let ctrl = controller::new();
                    let foo_profile = ctrl.profile_tx(FOO);
                    foo_profile.send(controller::profile(vec![
                        controller::route().request_path("/")
                            .label("hello", "foo"),
                    ], None, vec![]));
                    let bar_profile = ctrl.profile_tx(BAR);
                    bar_profile.send(controller::profile(vec![
                        controller::route().request_path("/")
                            .label("hello", "bar"),
                    ], None, vec![]));

                    let foo_eps = ctrl.destination_tx(FOO);
                    foo_eps.send_addr(foo.addr);
                    let bar_eps = ctrl.destination_tx(BAR);
                    bar_eps.send_addr(bar.addr);

                    Fixture {
                        foo_reqs, bar_reqs,
                        foo: Some(foo),
                        bar,
                        ctrl: Some(ctrl),
                        _foo_dst: (foo_profile, foo_eps),
                        _bar_dst: (bar_profile, bar_eps),
                    }
                }

                fn foo(&mut self) -> server::Listening {
                    self.foo.take().unwrap()
                }

                fn foo_reqs(&self) -> usize {
                    self.foo_reqs.load(Ordering::Acquire)
                }

                fn bar_reqs(&self) -> usize {
                    self.bar_reqs.load(Ordering::Acquire)
                }

                async fn proxy(&mut self) -> proxy::Proxy {
                    let ctrl = self.ctrl.take().unwrap();
                    proxy::new().controller(ctrl.run().await)
                }
            }

            async fn override_req(client: &client::Client) -> http::Response<hyper::Body> {
                client.request(
                    client.request_builder("/")
                        .header(OVERRIDE_HEADER, BAR)
                        .method("GET")
                ).await
                .expect("override request")
            }

            #[tokio::test(basic_scheduler)]
            async fn outbound_honors_override_header() {
                let mut fixture = Fixture::new().await;
                let proxy = fixture.proxy().await.run().await;

                let client = $make_client(proxy.outbound, FOO);

                // Request 1 --- without override header.
                assert_eq!(client.get("/").await, "hello from foo");
                assert_eq!(fixture.foo_reqs(), 1);
                assert_eq!(fixture.bar_reqs(), 0);

                // Request 2 --- with override header
                let res = override_req(&client).await;
                assert_eq!(res.status(), http::StatusCode::OK);
                let stream = res.into_parts().1;
                let mut body = hyper::body::aggregate(stream).await.expect("response 2 body");
                let body = std::str::from_utf8(body.to_bytes().as_ref()).expect("body is utf-8").to_owned();
                assert_eq!(body, "hello from bar");
                assert_eq!(fixture.foo_reqs(), 1);
                assert_eq!(fixture.bar_reqs(), 1);

                // Request 3 --- without override header again.
                assert_eq!(client.get("/").await, "hello from foo");
                assert_eq!(fixture.foo_reqs(), 2);
                assert_eq!(fixture.bar_reqs(), 1);

                // Ensure panics are propagated.
                tokio::join!{
                    fixture.foo().join(), fixture.bar.join()
                };
            }

            #[tokio::test(basic_scheduler)]
            async fn outbound_overrides_profile() {
                let mut fixture = Fixture::new().await;
                let proxy = fixture.proxy().await.run().await;

                println!("make client: {}", FOO);
                let client = $make_client(proxy.outbound, FOO);
                let metrics = client::http1(proxy.metrics, "localhost");

                // Request 1 --- without override header.
                client.get("/").await;
                assert_eventually_contains!(metrics.get("/metrics").await, "rt_hello=\"foo\"");

                // Request 2 --- with override header
                let res = override_req(&client).await;
                assert_eq!(res.status(), http::StatusCode::OK);
                assert_eventually_contains!(metrics.get("/metrics").await, "rt_hello=\"bar\"");

                // Ensure panics are propagated.
                tokio::join!{
                    fixture.foo().join(), fixture.bar.join()
                };
            }

            #[tokio::test(basic_scheduler)]
            async fn outbound_honors_override_header_with_orig_dst() {
                let mut fixture = Fixture::new().await;
                let proxy = fixture.proxy().await
                    .outbound(fixture.foo())
                    .run().await;

                let client = $make_client(proxy.outbound, "foo.test.svc.cluster.local");

                // Request 1 --- without override header.
                assert_eq!(client.get("/").await, "hello from foo");
                assert_eq!(fixture.foo_reqs(), 1);
                assert_eq!(fixture.bar_reqs(), 0);

                // Request 2 --- with override header
                let res = override_req(&client).await;
                assert_eq!(res.status(), http::StatusCode::OK);
                let stream = res.into_parts().1;
                let mut body = hyper::body::aggregate(stream).await.expect("response 2 body");
                let body = std::str::from_utf8(body.to_bytes().as_ref()).expect("body is utf-8").to_owned();
                assert_eq!(body, "hello from bar");
                assert_eq!(fixture.foo_reqs(), 1);
                assert_eq!(fixture.bar_reqs(), 1);

                // Request 3 --- without override header again.
                assert_eq!(client.get("/").await, "hello from foo");
                assert_eq!(fixture.foo_reqs(), 2);
                assert_eq!(fixture.bar_reqs(), 1);

                // Ensure panics are propagated.
                tokio::join! {
                    proxy.join_servers(),
                    fixture.bar.join()
                };
            }

            #[tokio::test(basic_scheduler)]
            async fn inbound_overrides_profile() {
                let mut fixture = Fixture::new().await;
                let proxy = fixture.proxy().await
                    .inbound(fixture.foo())
                    .run().await;

                let client = $make_client(proxy.inbound, FOO);
                let metrics = client::http1(proxy.metrics, "localhost");

                // Request 1 --- without override header.
                client.get("/").await;
                assert_eventually_contains!(metrics.get("/metrics").await, "rt_hello=\"foo\"");

                // // Request 2 --- with override header
                // let res = override_req(&client);
                // assert_eq!(res.status(), http::StatusCode::OK);
                // assert_eventually_contains!(metrics.get("/metrics"), "rt_hello=\"bar\"");

                // Ensure panics are propagated.
                proxy.join_servers().await;
            }

            #[tokio::test(basic_scheduler)]
            async fn inbound_still_routes_to_orig_dst() {
                let mut fixture = Fixture::new().await;
                let proxy = fixture.proxy().await
                    .inbound(fixture.foo())
                    .run().await;

                let client = $make_client(proxy.inbound, "foo.test.svc.cluster.local");

                // Request 1 --- without override header.
                assert_eq!(client.get("/").await, "hello from foo");
                assert_eq!(fixture.foo_reqs(), 1);
                assert_eq!(fixture.bar_reqs(), 0);

                // Request 2 --- with override header
                let res = override_req(&client).await;
                assert_eq!(res.status(), http::StatusCode::OK);
                let stream = res.into_parts().1;
                let mut body = hyper::body::aggregate(stream).await.expect("response 2 body");
                let body = std::str::from_utf8(body.to_bytes().as_ref()).expect("body is utf-8").to_owned();
                assert_eq!(body, "hello from foo");
                assert_eq!(fixture.foo_reqs(), 2);
                assert_eq!(fixture.bar_reqs(), 0);

                // Request 3 --- without override header again.
                assert_eq!(client.get("/").await, "hello from foo");
                assert_eq!(fixture.foo_reqs(), 3);
                assert_eq!(fixture.bar_reqs(), 0);

                // Ensure panics are propagated.
                tokio::join! {
                    proxy.join_servers(),
                    fixture.bar.join()
                };
            }

        }
    }
}

mod http2 {
    use linkerd2_app_integration::*;

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
        let ctrl = controller::new();
        ctrl.profile_tx_default(host);
        let dst = ctrl.destination_tx(host);
        // Start by "knowing" the first server...
        dst.send_addr(srv1.addr);

        let proxy = proxy::new().controller(ctrl.run().await).run().await;
        let client = client::http2(proxy.outbound, host);
        let metrics = client::http1(proxy.metrics, "localhost");

        assert_eq!(client.get("/").await, "hello");

        // Simulate the first server falling over without discovery
        // knowing about it...
        srv1.join().await;
        tokio::task::yield_now().await;

        // Wait until the proxy has seen the `srv1` disconnect...
        assert_eventually_contains!(
            metrics.get("/metrics").await,
            "tcp_close_total{direction=\"outbound\",peer=\"dst\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",errno=\"\"} 1"
        );

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
    use linkerd2_app_integration::*;

    generate_tests! {
        server: server::http1, client: client::http1
    }

    mod absolute_uris {
        use linkerd2_app_integration::*;

        generate_tests! {
            server: server::http1,
            client: client::http1_absolute_uris
        }
    }
}

mod proxy_to_proxy {
    use super::*;

    #[tokio::test]
    async fn outbound_http1() {
        let _trace = trace_init();

        // Instead of a second proxy, this mocked h2 server will be the target.
        let srv = server::http2()
            .route_fn("/hint", |req| {
                assert_eq!(req.headers()["l5d-orig-proto"], "HTTP/1.1");
                Response::builder()
                    .header("l5d-orig-proto", "HTTP/1.1")
                    .body(Default::default())
                    .unwrap()
            })
            .run()
            .await;

        let ctrl = controller::new();
        ctrl.profile_tx_default("disco.test.svc.cluster.local");
        let dst = ctrl.destination_tx("disco.test.svc.cluster.local");
        dst.send_h2_hinted(srv.addr);

        let proxy = proxy::new().controller(ctrl.run().await).run().await;

        let client = client::http1(proxy.outbound, "disco.test.svc.cluster.local");

        let res = client
            .request(client.request_builder("/hint"))
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.version(), http::Version::HTTP_11);

        // Ensure panics are propagated.
        proxy.join_servers().await;
        srv.join().await;
    }

    #[tokio::test]
    async fn inbound_http1() {
        let _trace = trace_init();

        let srv = server::http1()
            .route_fn("/h1", |req| {
                assert_eq!(req.version(), http::Version::HTTP_11);
                assert!(
                    !req.headers().contains_key("l5d-orig-proto"),
                    "h1 server shouldn't receive l5d-orig-proto header"
                );
                Response::default()
            })
            .run()
            .await;

        let ctrl = controller::new();
        ctrl.profile_tx_default("disco.test.svc.cluster.local");

        let proxy = proxy::new()
            .controller(ctrl.run().await)
            .inbound(srv)
            .run()
            .await;

        // This client will be used as a mocked-other-proxy.
        let client = client::http2(proxy.inbound, "disco.test.svc.cluster.local");

        let res = client
            .request(
                client
                    .request_builder("/h1")
                    .header("l5d-orig-proto", "HTTP/1.1"),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.version(), http::Version::HTTP_2);

        // Ensure panics are propagated.
        proxy.join_servers().await;
    }

    #[tokio::test]
    async fn inbound_should_strip_l5d_client_id() {
        let _trace = trace_init();

        let srv = server::http1()
            .route_fn("/stripped", |req| {
                assert_eq!(req.headers().get("l5d-client-id"), None, "header is set");
                Response::default()
            })
            .run()
            .await;

        let ctrl = controller::new();
        ctrl.profile_tx_default("disco.test.svc.cluster.local");

        let proxy = proxy::new()
            .controller(ctrl.run().await)
            .inbound(srv)
            .run()
            .await;

        let client = client::http1(proxy.inbound, "disco.test.svc.cluster.local");

        let res = client
            .request(
                client
                    .request_builder("/stripped")
                    .header("l5d-client-id", "sneaky.sneaky"),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 200, "not successful");

        // Ensure panics are propagated.
        proxy.join_servers().await;
    }

    #[tokio::test]
    async fn outbound_should_strip_l5d_client_id() {
        let _trace = trace_init();

        let srv = server::http1()
            .route_fn("/stripped", |req| {
                assert_eq!(req.headers().get("l5d-client-id"), None);
                Response::default()
            })
            .run()
            .await;

        let ctrl = controller::new();
        ctrl.profile_tx_default("disco.test.svc.cluster.local");
        ctrl.destination_tx("disco.test.svc.cluster.local")
            .send_addr(srv.addr);
        let proxy = proxy::new().controller(ctrl.run().await).run().await;

        let client = client::http1(proxy.outbound, "disco.test.svc.cluster.local");

        let res = client
            .request(
                client
                    .request_builder("/stripped")
                    .header("l5d-client-id", "sneaky.sneaky"),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 200);

        // Ensure panics are propagated.
        proxy.join_servers().await;
    }

    #[tokio::test]
    async fn inbound_should_strip_l5d_server_id() {
        let _trace = trace_init();

        let srv = server::http1()
            .route_fn("/strip-me", |_req| {
                Response::builder()
                    .header("l5d-server-id", "i'm not from the proxy!")
                    .body(Default::default())
                    .unwrap()
            })
            .run()
            .await;

        let ctrl = controller::new();
        ctrl.profile_tx_default("disco.test.svc.cluster.local");

        let proxy = proxy::new()
            .controller(ctrl.run().await)
            .inbound(srv)
            .run()
            .await;

        let client = client::http1(proxy.inbound, "disco.test.svc.cluster.local");

        let res = client
            .request(client.request_builder("/strip-me"))
            .await
            .unwrap();
        assert_eq!(res.status(), 200, "must be sucessful");
        assert_eq!(
            res.headers().get("l5d-server-id"),
            None,
            "header must not exist"
        );
    }

    #[tokio::test]
    async fn outbound_should_strip_l5d_server_id() {
        let _trace = trace_init();

        let srv = server::http1()
            .route_fn("/strip-me", |_req| {
                Response::builder()
                    .header("l5d-server-id", "i'm not from the proxy!")
                    .body(Default::default())
                    .unwrap()
            })
            .run()
            .await;

        let ctrl = controller::new();
        ctrl.destination_tx("disco.test.svc.cluster.local")
            .send_addr(srv.addr);
        ctrl.profile_tx_default("disco.test.svc.cluster.local");
        let proxy = proxy::new().controller(ctrl.run().await).run().await;

        let client = client::http1(proxy.outbound, "disco.test.svc.cluster.local");

        let res = client
            .request(client.request_builder("/strip-me"))
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.headers().get("l5d-server-id"), None);
    }

    macro_rules! generate_l5d_tls_id_test {
        (server: $make_server:path, client: $make_client:path) => {
            let _trace = trace_init();
            let id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
            let id_env = identity::Identity::new("foo-ns1", id.to_string());

            let srv = $make_server()
                .route_fn("/hallo", move |req| {
                    assert_eq!(req.headers()["l5d-client-id"], id);
                    Response::default()
                })
                .run()
                .await;

            let in_proxy = proxy::new()
                .inbound(srv)
                .identity(id_env.service().run().await)
                .run_with_test_env(id_env.env.clone())
                .await;

            let ctrl = controller::new();
            let dst = ctrl.destination_tx("disco.test.svc.cluster.local");
            dst.send(controller::destination_add_tls(in_proxy.inbound, id));

            let out_proxy = proxy::new()
                .controller(ctrl.run().await)
                .identity(id_env.service().run().await)
                .run_with_test_env(id_env.env.clone())
                .await;

            let client = $make_client(out_proxy.outbound, "disco.test.svc.cluster.local");

            let res = client
                .request(client.request_builder("/hallo"))
                .await
                .unwrap();
            assert_eq!(res.status(), 200);
            assert_eq!(res.headers()["l5d-server-id"], id);

            tokio::join! { in_proxy.join_servers(), out_proxy.join_servers() };
        };
    }

    #[tokio::test]
    #[ignore] // #2597
    async fn outbound_http1_l5d_server_id_l5d_client_id() {
        generate_l5d_tls_id_test! {
            server: server::http1,
            client: client::http1
        }
    }

    #[tokio::test]
    #[ignore] // #2597
    async fn outbound_http2_l5d_server_id_l5d_client_id() {
        generate_l5d_tls_id_test! {
            server: server::http2,
            client: client::http2
        }
    }
}
