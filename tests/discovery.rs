#![deny(warnings)]
#![recursion_limit = "128"]
#[macro_use]
mod support;
use self::support::*;

macro_rules! generate_tests {
    (server: $make_server:path, client: $make_client:path) => {
        use linkerd2_proxy_api as pb;

        #[test]
        fn outbound_asks_controller_api() {
            let _ = env_logger_init();
            let srv = $make_server().route("/", "hello").route("/bye", "bye").run();

            let ctrl = controller::new()
                .destination_and_close("disco.test.svc.cluster.local", srv.addr);

            let proxy = proxy::new().controller(ctrl.run()).outbound(srv).run();
            let client = $make_client(proxy.outbound, "disco.test.svc.cluster.local");

            assert_eq!(client.get("/"), "hello");
            assert_eq!(client.get("/bye"), "bye");
        }

        #[test]
        fn outbound_router_capacity() {
            let _ = env_logger_init();
            let srv = $make_server().route("/", "hello").run();
            let srv_addr = srv.addr;

            let mut env = app::config::TestEnv::new();

            // Testing what happens if we go over the router capacity...
            let router_cap = 2;
            env.put(app::config::ENV_OUTBOUND_ROUTER_CAPACITY, router_cap.to_string());

            let ctrl = controller::new();
            let _txs = (0..=router_cap).map(|n| {
                let disco_n = format!("disco{}.test.svc.cluster.local", n);
                let tx = ctrl.destination_tx(&disco_n);
                tx.send_addr(srv_addr);
                tx // This will go into a vec, to keep the stream open.
            }).collect::<Vec<_>>();

            let proxy = proxy::new()
                .controller(ctrl.run())
                .outbound(srv)
                .run_with_test_env(env);

            // Make requests that go through service discovery, to reach the
            // router capacity.
            for n in 0..router_cap {
                let route = format!("disco{}.test.svc.cluster.local", n);
                let client = $make_client(proxy.outbound, route);
                println!("trying disco{}...", n);
                assert_eq!(client.get("/"), "hello");
            }

            // The next request will fail, because we have reached the
            // router capacity.
            let nth_host = format!("disco{}.test.svc.cluster.local", router_cap);
            let client = $make_client(proxy.outbound, &*nth_host);
            println!("disco{} should fail...", router_cap);
            let rsp = client.request(&mut client.request_builder("/"));

            // We should have gotten an HTTP response, not an error.
            assert_eq!(rsp.status(), http::StatusCode::SERVICE_UNAVAILABLE);
        }

        #[test]
        fn outbound_reconnects_if_controller_stream_ends() {
            let _ = env_logger_init();

            let srv = $make_server().route("/recon", "nect").run();

            let ctrl = controller::new()
                .destination_close("disco.test.svc.cluster.local")
                .destination_and_close("disco.test.svc.cluster.local", srv.addr);

            let proxy = proxy::new().controller(ctrl.run()).outbound(srv).run();
            let client = $make_client(proxy.outbound, "disco.test.svc.cluster.local");

            assert_eq!(client.get("/recon"), "nect");
        }

        #[test]
        fn outbound_falls_back_to_orig_dst_when_destination_has_no_endpoints() {
            let _ = env_logger_init();

            let srv = $make_server().route("/", "hello").run();

            let ctrl = controller::new();
            ctrl.destination_tx("disco.test.svc.cluster.local")
                .send(controller::destination_exists_with_no_endpoints());

            let proxy = proxy::new()
                .controller(ctrl.run())
                .outbound(srv)
                .run();

            let client = $make_client(proxy.outbound, "disco.test.svc.cluster.local");

            assert_eq!(client.get("/"), "hello");
        }

        #[test]
        fn outbound_falls_back_to_orig_dst_when_destination_doesnt_exist() {
            let _ = env_logger_init();

            let srv = $make_server().route("/", "hello").run();

            let ctrl = controller::new();
            ctrl.destination_tx("disco.test.svc.cluster.local")
                .send(controller::destination_does_not_exist());

            let proxy = proxy::new()
                .controller(ctrl.run())
                .outbound(srv)
                .run();

            let client = $make_client(proxy.outbound, "disco.test.svc.cluster.local");

            assert_eq!(client.get("/"), "hello");
        }

        #[test]
        fn outbound_does_not_reconnect_after_invalid_argument() {
            let _ = env_logger_init();

            let srv = $make_server().route("/", "hello").run();

            let ctrl = controller::new()
                .destination_err(
                    "disco.test.svc.cluster.local",
                    grpc::Code::InvalidArgument,
                )
                // The test controller will panic if the proxy tries to talk to it again.
                .no_more_destinations();

            let proxy = proxy::new()
                .controller(ctrl.run())
                .outbound(srv)
                .run();

            let client = $make_client(proxy.outbound, "disco.test.svc.cluster.local");

            assert_eq!(client.get("/"), "hello");
        }

        #[test]
        fn outbound_destinations_reset_on_reconnect_followed_by_add_none() {
            outbound_destinations_reset_on_reconnect(
                controller::destination_add_none()
            )
        }

        #[test]
        fn outbound_destinations_reset_on_reconnect_followed_by_remove_none() {
            outbound_destinations_reset_on_reconnect(
                controller::destination_remove_none()
            )
        }

        fn init_env() -> app::config::TestEnv {
            let _ = env_logger_init();
            app::config::TestEnv::new()
        }

        fn outbound_destinations_reset_on_reconnect(up: pb::destination::Update) {
            use std::thread;

            let env = init_env();
            let srv = $make_server().route("/", "hello").run();
            let ctrl = controller::new();

            let dst_tx0 = ctrl.destination_tx("initially-exists.ns.svc.cluster.local");
            dst_tx0.send_addr(srv.addr);

            let dst_tx1 = ctrl.destination_tx("initially-exists.ns.svc.cluster.local");

            let proxy = proxy::new()
                .controller(ctrl.run())
                .outbound(srv)
                .run_with_test_env(env);

            let initially_exists =
                $make_client(proxy.outbound, "initially-exists.ns.svc.cluster.local");
            assert_eq!(initially_exists.get("/"), "hello");

            drop(dst_tx0); // trigger reconnect
            dst_tx1.send(up);

            // Wait for the reconnect to happen. TODO: Replace this flaky logic.
            thread::sleep(Duration::from_millis(1000));


            // This would wait since there are no endpoints.
            let mut req = initially_exists.request_builder("/");
            initially_exists
                .request_async(req.method("GET"))
                .wait_timeout(Duration::from_secs(1))
                .expect_timedout("request should wait for destination capacity");
        }

        #[test]
        #[ignore] //TODO: there's currently no destination-acquisition timeout...
        fn outbound_times_out() {
            let env = init_env();

            let srv = $make_server().route("/hi", "hello").run();
            let ctrl = controller::new();

            // when the proxy requests the destination, don't respond.
            let _dst_tx = ctrl.destination_tx("disco.test.svc.cluster.local");

            let proxy = proxy::new()
                .controller(ctrl.run())
                .outbound(srv)
                .run_with_test_env(env);

            let client = $make_client(proxy.outbound, "disco.test.svc.cluster.local");
            let mut req = client.request_builder("/");
            let rsp = client.request(req.method("GET"));
            // the request should time out
            assert_eq!(rsp.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
        }

        #[test]
        fn outbound_asks_controller_without_orig_dst() {
            let _ = init_env();

            let srv = $make_server()
                .route("/", "hello")
                .route("/bye", "bye")
                .run();

            let ctrl = controller::new()
                .destination_and_close("disco.test.svc.cluster.local", srv.addr);

            let proxy = proxy::new()
                .controller(ctrl.run())
                // don't set srv as outbound(), so that SO_ORIGINAL_DST isn't
                // used as a backup
                .run();
            let client = $make_client(proxy.outbound, "disco.test.svc.cluster.local");

            assert_eq!(client.get("/"), "hello");
            assert_eq!(client.get("/bye"), "bye");
        }

        #[test]
        fn outbound_skips_controller_if_destination_is_address() {
            let _ = init_env();

            let srv = $make_server()
                .route("/", "hello")
                .route("/bye", "bye")
                .run();

            let host = srv.addr.to_string();

            // don't set outbound() or controller()
            let proxy = proxy::new()
                .run();
            let client = $make_client(proxy.outbound, &*host);

            assert_eq!(client.get("/"), "hello");
            assert_eq!(client.get("/bye"), "bye");
        }

        #[test]
        fn outbound_error_reconnects_after_backoff() {
            let mut env = init_env();

            let srv = $make_server()
                .route("/", "hello")
                .run();

            // Used to delay `listen` in the server, to force connection refused errors.
            let (tx, rx) = oneshot::channel();

            let ctrl = controller::new();

            let dst_tx = ctrl.destination_tx("disco.test.svc.cluster.local");
            dst_tx.send_addr(srv.addr);
            // but don't drop, to not trigger stream closing reconnects

            env.put(app::config::ENV_CONTROL_EXP_BACKOFF_MIN, "100ms".to_owned());
            env.put(app::config::ENV_CONTROL_EXP_BACKOFF_MAX, "300ms".to_owned());
            env.put(app::config::ENV_CONTROL_EXP_BACKOFF_JITTER, "0.1".to_owned());

            let proxy = proxy::new()
                .controller(ctrl.delay_listen(rx.map_err(|_| ())))
                // don't set srv as outbound(), so that SO_ORIGINAL_DST isn't
                // used as a backup
                .run_with_test_env(env);

            // Allow the control client to notice a connection error
            ::std::thread::sleep(Duration::from_millis(500));

            // Allow our controller to start accepting connections,
            // and then wait a little bit so the client tries again.
            drop(tx);
            ::std::thread::sleep(Duration::from_millis(500));

            let client = $make_client(proxy.outbound, "disco.test.svc.cluster.local");

            assert_eq!(client.get("/"), "hello");
        }

        mod remote_header {
            use http::header::HeaderValue;
            use super::super::*;

            const REMOTE_IP_HEADER: &'static str = "l5d-remote-ip";
            const IP_1: &'static str = "0.0.0.0";
            const IP_2: &'static str = "127.0.0.1";

            #[test]
            fn outbound_should_strip() {
                let _ = env_logger_init();
                let header = HeaderValue::from_static(IP_1);

                let srv = $make_server().route_fn("/strip", |_req| {
                    Response::builder().header(REMOTE_IP_HEADER, IP_1).body(Default::default()).unwrap()
                }).run();

                let ctrl = controller::new().destination_and_close("disco.test.svc.cluster.local", srv.addr);
                let proxy = proxy::new().controller(ctrl.run()).outbound(srv).run();
                let client = $make_client(proxy.outbound, "disco.test.svc.cluster.local");
                let rsp = client.request(&mut client.request_builder("/strip"));

                assert_eq!(rsp.status(), 200);
                assert_ne!(rsp.headers().get(REMOTE_IP_HEADER), Some(&header));
            }

            #[test]
            fn inbound_should_strip() {
                let _ = env_logger_init();
                let header = HeaderValue::from_static(IP_1);

                let srv = $make_server().route_fn("/strip", move |req| {
                    assert_ne!(req.headers().get(REMOTE_IP_HEADER), Some(&header));
                    Response::default()
                }).run();

                let proxy = proxy::new().inbound(srv).run();
                let client = $make_client(proxy.inbound, "disco.test.svc.cluster.local");
                let rsp = client.request(client.request_builder("/strip").header(REMOTE_IP_HEADER, IP_1));

                assert_eq!(rsp.status(), 200);
            }

            #[test]
            #[ignore] // #2597
            fn outbound_should_set() {
                let _ = env_logger_init();
                let header = HeaderValue::from_static(IP_2);

                let srv = $make_server().route("/set", "hello").run();
                let ctrl = controller::new().destination_and_close("disco.test.svc.cluster.local", srv.addr);
                let proxy = proxy::new().controller(ctrl.run()).outbound(srv).run();
                let client = $make_client(proxy.outbound, "disco.test.svc.cluster.local");
                let rsp = client.request(&mut client.request_builder("/set"));

                assert_eq!(rsp.status(), 200);
                assert_eq!(rsp.headers().get(REMOTE_IP_HEADER), Some(&header));
            }

            #[test]
            #[ignore] // #2597
            fn inbound_should_set() {
                let _ = env_logger_init();

                let header = HeaderValue::from_static(IP_2);

                let srv = $make_server().route_fn("/set", move |req| {
                    assert_eq!(req.headers().get(REMOTE_IP_HEADER), Some(&header));
                    Response::default()
                }).run();

                let proxy = proxy::new().inbound(srv).run();
                let client = $make_client(proxy.inbound, "disco.test.svc.cluster.local");
                let rsp = client.request(&mut client.request_builder("/set"));

                assert_eq!(rsp.status(), 200);
            }
        }

        mod override_header {
            use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
            use super::super::*;

            const OVERRIDE_HEADER: &'static str = "l5d-dst-override";
            const FOO: &'static str = "foo.test.svc.cluster.local";
            const BAR: &'static str = "bar.test.svc.cluster.local";

            struct Fixture {
                foo_reqs: Arc<AtomicUsize>,
                bar_reqs: Arc<AtomicUsize>,
                foo: Option<server::Listening>,
                _bar: server::Listening,
                ctrl: Option<controller::Controller>,

            }

            impl Fixture {
                fn new() -> Fixture {
                    let _ = env_logger_init();

                    let foo_reqs = Arc::new(AtomicUsize::new(0));
                    let foo_reqs2 = foo_reqs.clone();
                    let foo = $make_server().route_fn("/", move |req| {
                        assert!(
                            !req.headers().contains_key(OVERRIDE_HEADER),
                            "dst override header should be stripped before forwarding request",
                        );
                        foo_reqs2.clone().fetch_add(1, Ordering::Release);
                        Response::builder().status(200)
                            .body(Bytes::from_static(&b"hello from foo"[..]))
                            .unwrap()
                    }).run();

                    let bar_reqs = Arc::new(AtomicUsize::new(0));
                    let bar_reqs2 = bar_reqs.clone();
                    let bar = $make_server().route_fn("/", move |req| {
                        assert!(
                            !req.headers().contains_key(OVERRIDE_HEADER),
                            "dst override header should be stripped before forwarding request",
                        );
                        bar_reqs2.clone().fetch_add(1, Ordering::Release);
                        Response::builder().status(200)
                            .body(Bytes::from_static(&b"hello from bar"[..]))
                            .unwrap()
                    }).run();

                    let ctrl = controller::new()
                        .destination_and_close(FOO, foo.addr)
                        .destination_and_close(BAR, bar.addr);
                    Fixture {
                        foo_reqs, bar_reqs,
                        foo: Some(foo),
                        _bar: bar,
                        ctrl: Some(ctrl)
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

                fn proxy(&mut self) -> proxy::Proxy {
                    let ctrl = self.ctrl.take().unwrap();
                    proxy::new().controller(ctrl.run())
                }
            }

            fn override_req(client: &client::Client) -> http::Response<client::BytesBody> {
                client.request(
                    client.request_builder("/")
                        .header(OVERRIDE_HEADER, BAR)
                        .method("GET")
                )
            }

            #[test]
            fn outbound_honors_override_header() {
                let mut fixture = Fixture::new();
                let proxy = fixture.proxy().run();

                let client = $make_client(proxy.outbound, FOO);

                // Request 1 --- without override header.
                assert_eq!(client.get("/"), "hello from foo");
                assert_eq!(fixture.foo_reqs(), 1);
                assert_eq!(fixture.bar_reqs(), 0);

                // Request 2 --- with override header
                let res = override_req(&client);
                assert_eq!(res.status(), http::StatusCode::OK);
                let stream = res.into_parts().1;
                let body = stream.concat2()
                    .map(|body| ::std::str::from_utf8(&body).unwrap().to_string())
                    .wait()
                    .expect("response 2 body");
                assert_eq!(body, "hello from bar");
                assert_eq!(fixture.foo_reqs(), 1);
                assert_eq!(fixture.bar_reqs(), 1);

                // Request 3 --- without override header again.
                assert_eq!(client.get("/"), "hello from foo");
                assert_eq!(fixture.foo_reqs(), 2);
                assert_eq!(fixture.bar_reqs(), 1);
            }

            #[test]
            fn outbound_honors_override_header_with_orig_dst() {
                let mut fixture = Fixture::new();
                let proxy = fixture.proxy()
                    .outbound(fixture.foo())
                    .run();

                let client = $make_client(proxy.outbound, "foo.test.svc.cluster.local");

                // Request 1 --- without override header.
                assert_eq!(client.get("/"), "hello from foo");
                assert_eq!(fixture.foo_reqs(), 1);
                assert_eq!(fixture.bar_reqs(), 0);

                // Request 2 --- with override header
                let res = override_req(&client);
                assert_eq!(res.status(), http::StatusCode::OK);
                let stream = res.into_parts().1;
                let body = stream.concat2()
                    .map(|body| ::std::str::from_utf8(&body).unwrap().to_string())
                    .wait()
                    .expect("response 2 body");
                assert_eq!(body, "hello from bar");
                assert_eq!(fixture.foo_reqs(), 1);
                assert_eq!(fixture.bar_reqs(), 1);

                // Request 3 --- without override header again.
                assert_eq!(client.get("/"), "hello from foo");
                assert_eq!(fixture.foo_reqs(), 2);
                assert_eq!(fixture.bar_reqs(), 1);
            }

            #[test]
            fn inbound_does_not_honor_override_header() {
                let mut fixture = Fixture::new();
                let proxy = fixture.proxy()
                    .inbound(fixture.foo())
                    .run();

                let client = $make_client(proxy.inbound, "foo.test.svc.cluster.local");

                // Request 1 --- without override header.
                assert_eq!(client.get("/"), "hello from foo");
                assert_eq!(fixture.foo_reqs(), 1);
                assert_eq!(fixture.bar_reqs(), 0);

                // Request 2 --- with override header
                let res = override_req(&client);
                assert_eq!(res.status(), http::StatusCode::OK);
                let stream = res.into_parts().1;
                let body = stream.concat2()
                    .map(|body| ::std::str::from_utf8(&body).unwrap().to_string())
                    .wait()
                    .expect("response 2 body");
                assert_eq!(body, "hello from foo");
                assert_eq!(fixture.foo_reqs(), 2);
                assert_eq!(fixture.bar_reqs(), 0);

                // Request 3 --- without override header again.
                assert_eq!(client.get("/"), "hello from foo");
                assert_eq!(fixture.foo_reqs(), 3);
                assert_eq!(fixture.bar_reqs(), 0);
            }

        }
    }
}

mod http2 {
    use super::support::*;

    generate_tests! { server: server::new, client: client::new }

    #[test]
    fn outbound_balancer_waits_for_ready_endpoint() {
        // See https://github.com/linkerd/linkerd2/issues/2550
        let _ = env_logger_init();

        let srv1 = server::http2()
            .route("/", "hello")
            .route("/bye", "bye")
            .run();

        let srv2 = server::http2()
            .route("/", "hello")
            .route("/bye", "bye")
            .run();

        let host = "disco.test.svc.cluster.local";
        let ctrl = controller::new();
        let dst = ctrl.destination_tx(host);
        // Start by "knowing" the first server...
        dst.send_addr(srv1.addr);

        let proxy = proxy::new().controller(ctrl.run()).run();
        let client = client::http2(proxy.outbound, host);
        let metrics = client::http1(proxy.metrics, "localhost");

        assert_eq!(client.get("/"), "hello");

        // Simulate the first server falling over without discovery
        // knowing about it...
        drop(srv1);

        // Wait until the proxy has seen the `srv1` disconnect...
        assert_eventually_contains!(
            metrics.get("/metrics"),
            "tcp_close_total{direction=\"outbound\",peer=\"dst\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",errno=\"\"} 1"
        );

        // Start a new request to the destination, now that the server is dead.
        // This request should be waiting at the balancer for a ready endpoint.
        //
        // The only one it knows about is dead, so it won't have progressed.
        let fut = client.request_async(&mut client.request_builder("/bye"));

        // When we tell the balancer about a new endpoint, it should have added
        // it and then dispatched the request...
        dst.send_addr(srv2.addr);

        let res = fut.wait().expect("/bye response");
        assert_eq!(res.status(), http::StatusCode::OK);
    }
}

mod http1 {
    use super::support::*;

    generate_tests! {
        server: server::http1, client: client::http1
    }

    mod absolute_uris {
        use super::super::support::*;

        generate_tests! {
            server: server::http1,
            client: client::http1_absolute_uris
        }
    }

}

mod proxy_to_proxy {
    use super::*;

    #[test]
    fn outbound_http1() {
        let _ = env_logger_init();

        // Instead of a second proxy, this mocked h2 server will be the target.
        let srv = server::http2()
            .route_fn("/hint", |req| {
                assert_eq!(req.headers()["l5d-orig-proto"], "HTTP/1.1");
                Response::builder()
                    .header("l5d-orig-proto", "HTTP/1.1")
                    .body(Default::default())
                    .unwrap()
            })
            .run();

        let ctrl = controller::new();
        let dst = ctrl.destination_tx("disco.test.svc.cluster.local");
        dst.send_h2_hinted(srv.addr);

        let proxy = proxy::new().controller(ctrl.run()).run();

        let client = client::http1(proxy.outbound, "disco.test.svc.cluster.local");

        let res = client.request(&mut client.request_builder("/hint"));
        assert_eq!(res.status(), 200);
        assert_eq!(res.version(), http::Version::HTTP_11);
    }

    #[test]
    fn inbound_http1() {
        let _ = env_logger_init();

        let srv = server::http1()
            .route_fn("/h1", |req| {
                assert_eq!(req.version(), http::Version::HTTP_11);
                assert!(
                    !req.headers().contains_key("l5d-orig-proto"),
                    "h1 server shouldn't receive l5d-orig-proto header"
                );
                Response::default()
            })
            .run();

        let ctrl = controller::new();

        let proxy = proxy::new().controller(ctrl.run()).inbound(srv).run();

        // This client will be used as a mocked-other-proxy.
        let client = client::http2(proxy.inbound, "disco.test.svc.cluster.local");

        let res = client.request(
            client
                .request_builder("/h1")
                .header("l5d-orig-proto", "HTTP/1.1"),
        );
        assert_eq!(res.status(), 200);
        assert_eq!(res.version(), http::Version::HTTP_2);
    }

    #[test]
    fn inbound_should_strip_l5d_client_id() {
        let _ = env_logger_init();

        let srv = server::http1()
            .route_fn("/stripped", |req| {
                assert_eq!(req.headers().get("l5d-client-id"), None);
                Response::default()
            })
            .run();

        let proxy = proxy::new().inbound(srv).run();

        let client = client::http1(proxy.inbound, "disco.test.svc.cluster.local");

        let res = client.request(
            client
                .request_builder("/stripped")
                .header("l5d-client-id", "sneaky.sneaky"),
        );
        assert_eq!(res.status(), 200);
    }

    #[test]
    fn outbound_should_strip_l5d_client_id() {
        let _ = env_logger_init();

        let srv = server::http1()
            .route_fn("/stripped", |req| {
                assert_eq!(req.headers().get("l5d-client-id"), None);
                Response::default()
            })
            .run();

        let ctrl = controller::new()
            .destination_and_close("disco.test.svc.cluster.local", srv.addr)
            .run();
        let proxy = proxy::new().controller(ctrl).run();

        let client = client::http1(proxy.outbound, "disco.test.svc.cluster.local");

        let res = client.request(
            client
                .request_builder("/stripped")
                .header("l5d-client-id", "sneaky.sneaky"),
        );
        assert_eq!(res.status(), 200);
    }

    #[test]
    fn inbound_should_strip_l5d_server_id() {
        let _ = env_logger_init();

        let srv = server::http1()
            .route_fn("/strip-me", |_req| {
                Response::builder()
                    .header("l5d-server-id", "i'm not from the proxy!")
                    .body(Default::default())
                    .unwrap()
            })
            .run();

        let proxy = proxy::new().inbound(srv).run();

        let client = client::http1(proxy.inbound, "disco.test.svc.cluster.local");

        let res = client.request(&mut client.request_builder("/strip-me"));
        assert_eq!(res.status(), 200);
        assert_eq!(res.headers().get("l5d-server-id"), None);
    }

    #[test]
    fn outbound_should_strip_l5d_server_id() {
        let _ = env_logger_init();

        let srv = server::http1()
            .route_fn("/strip-me", |_req| {
                Response::builder()
                    .header("l5d-server-id", "i'm not from the proxy!")
                    .body(Default::default())
                    .unwrap()
            })
            .run();

        let ctrl = controller::new()
            .destination_and_close("disco.test.svc.cluster.local", srv.addr)
            .run();
        let proxy = proxy::new().controller(ctrl).run();

        let client = client::http1(proxy.outbound, "disco.test.svc.cluster.local");

        let res = client.request(&mut client.request_builder("/strip-me"));
        assert_eq!(res.status(), 200);
        assert_eq!(res.headers().get("l5d-server-id"), None);
    }

    macro_rules! generate_l5d_tls_id_test {
        (server: $make_server:path, client: $make_client:path) => {
            let _ = env_logger_init();
            let id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
            let id_env = identity::Identity::new("foo-ns1", id.to_string());

            let srv = $make_server()
                .route_fn("/hallo", move |req| {
                    assert_eq!(req.headers()["l5d-client-id"], id);
                    Response::default()
                })
                .run();

            let in_proxy = proxy::new()
                .inbound(srv)
                .identity(id_env.service().run())
                .run_with_test_env(id_env.env.clone());

            let ctrl = controller::new();
            let dst = ctrl.destination_tx("disco.test.svc.cluster.local");
            dst.send(controller::destination_add_tls(in_proxy.inbound, id));

            let out_proxy = proxy::new()
                .controller(ctrl.run())
                .identity(id_env.service().run())
                .run_with_test_env(id_env.env.clone());

            let client = $make_client(out_proxy.outbound, "disco.test.svc.cluster.local");

            let res = client.request(&mut client.request_builder("/hallo"));
            assert_eq!(res.status(), 200);
            assert_eq!(res.headers()["l5d-server-id"], id);
        };
    }

    #[test]
    #[ignore] // #2597
    fn outbound_http1_l5d_server_id_l5d_client_id() {
        generate_l5d_tls_id_test! {
            server: server::http1,
            client: client::http1
        }
    }

    #[test]
    #[ignore] // #2597
    fn outbound_http2_l5d_server_id_l5d_client_id() {
        generate_l5d_tls_id_test! {
            server: server::http2,
            client: client::http2
        }
    }
}
