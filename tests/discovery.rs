#![deny(warnings)]
#![recursion_limit = "128"]
#[macro_use]
mod support;
use self::support::*;

// This test has to be generated separately, since we want it to run for
// HTTP/1, but not for HTTP/2. This is because the test uses httpbin.org
// as an external DNS name, and it will 500 our H2 requests because it
// requires prior knowledge.
macro_rules! generate_outbound_dns_limit_test {
    (server: $make_server:path, client: $make_client:path) => {
        #[test]
        fn outbound_dest_limit_does_not_limit_dns() {
            let _ = env_logger_init();
            let srv = $make_server().route("/", "hello").run();
            let srv_addr = srv.addr;

            let mut env = app::config::TestEnv::new();
            env.put(
                app::config::ENV_DESTINATION_CLIENT_CONCURRENCY_LIMIT,
                "2".to_owned(),
            );

            let ctrl = controller::new();
            let _txs = (1..=3)
                .map(|n| {
                    let disco_n = format!("disco{}.test.svc.cluster.local", n);
                    let tx = ctrl.destination_tx(&disco_n);
                    tx.send_addr(srv_addr);
                    tx // This will go into a vec, to keep the stream open.
                })
                .collect::<Vec<_>>();

            let proxy = proxy::new()
                .controller(ctrl.run())
                .outbound(srv)
                .run_with_test_env(env);

            // Make two requests that go through service discovery, to reach the
            // maximum number of Destination resolutions.
            for n in 1..=2 {
                let route = format!("disco{}.test.svc.cluster.local", n);
                let client = $make_client(proxy.outbound, route);
                println!("trying {}th destination...", n);
                assert_eq!(client.get("/"), "hello");
            }

            // The next request will fail, because we have reached the
            // Destination resolution limit, but we have _not_ reached the
            // route cache capacity, so we won't start evicting inactive
            // routes yet, and their Destination resolutions will remain
            // active.
            let client = $make_client(proxy.outbound, "disco3.test.svc.cluster.local");
            println!("trying 3rd destination...");
            let mut req = client.request_builder("/");
            client
                .request_async(req.method("GET"))
                .wait_timeout(Duration::from_secs(1))
                .expect_timedout("request should wait for destination capacity");

            // However, a request for a DNS name that _doesn't_ go through the
            // Destination service should not be affected by the limit.
            // TODO: The controller should also assert the proxy doesn't make
            // requests when it has hit the maximum number of resolutions.
            let client = $make_client(proxy.outbound, "httpbin.org");
            println!("trying httpbin.org...");
            let mut req = client.request_builder("/");
            let rsp = client.request(req.method("GET"));
            assert_eq!(rsp.status(), http::StatusCode::OK);
        }
    };
}

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
        fn outbound_dest_concurrency_limit() {
            let _ = env_logger_init();
            let srv = $make_server().route("/", "hello").run();
            let srv_addr = srv.addr;

            let mut env = app::config::TestEnv::new();

            // Set the maximum number of Destination service resolutions
            // lower than the total router capacity, so we can reach the
            // maximum number of destinations before we start evicting
            // routes.
            let num_dests = 4;
            env.put(app::config::ENV_DESTINATION_CLIENT_CONCURRENCY_LIMIT, (num_dests - 1).to_string());
            env.put(app::config::ENV_OUTBOUND_ROUTER_CAPACITY, num_dests.to_string());
            env.put(app::config::ENV_OUTBOUND_ROUTER_MAX_IDLE_AGE, "1s".to_owned());

            let ctrl = controller::new();
            let _txs = (1..=num_dests).map(|n| {
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
            // maximum number of Destination resolutions.
            for n in 1..num_dests {
                let route = format!("disco{}.test.svc.cluster.local", n);
                let client = $make_client(proxy.outbound, route);
                println!("trying {}th destination...", n);
                assert_eq!(client.get("/"), "hello");
            }

            // The next request will fail, because we have reached the
            // Destination resolution limit, but we have _not_ reached the
            // route cache capacity, so we won't start evicting inactive
            // routes yet, and their Destination resolutions will remain
            // active.
            let nth_host = format!("disco{}.test.svc.cluster.local", num_dests);
            let client = $make_client(proxy.outbound, &*nth_host);
            println!("trying {}th destination...", num_dests);
            let mut req = client.request_builder("/");
            client
                .request_async(req.method("GET"))
                .wait_timeout(Duration::from_secs(1))
                .expect_timedout("request should wait for destination capacity");

            // TODO: The controller should also assert the proxy doesn't make
            // requests when it has hit the maximum number of resolutions.

            // Sleep for longer than the router cache's max idle age, so that
            //  at least one old route will be considered inactive.
            ::std::thread::sleep(Duration::from_secs(2));

            // Next, make a request that _won't_ trigger a Destination lookup,
            // so that we can reach the router's capacity as well.
            let addr_client = $make_client(proxy.outbound, format!("{}", srv_addr));
            println!("trying {} directly...", srv_addr);
            assert_eq!(addr_client.get("/"), "hello");

            // Now that the router cache capacity has been reached, subsequent
            // requests that add a new route will evict an inactive route. This
            // should drop their Destination resolutions, so we should now be
            // able to open a new one.
            println!("trying {}th destination again...", num_dests);
            let client = $make_client(proxy.outbound, &*nth_host);
            assert_eq!(client.get("/"), "hello");
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
        fn outbound_destinations_reset_on_reconnect_followed_by_no_endpoints_exists() {
            outbound_destinations_reset_on_reconnect(
                controller::destination_exists_with_no_endpoints()
            )
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

            env.put(app::config::ENV_CONTROL_BACKOFF_DELAY, "100ms".to_owned());

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
            const FOO: &'static str = "0.0.0.0";
            const BAR: &'static str = "127.0.0.1";

            #[test]
            fn outbound_should_strip() {
                let _ = env_logger_init();
                let header = HeaderValue::from_static(FOO);

                let srv = $make_server().route_fn("/strip", |_req| {
                    Response::builder().header(REMOTE_IP_HEADER, FOO).body(Default::default()).unwrap()
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
                let header = HeaderValue::from_static(FOO);

                let srv = $make_server().route_fn("/strip", move |req| {
                    assert_ne!(req.headers().get(REMOTE_IP_HEADER), Some(&header));
                    Response::default()
                }).run();

                let proxy = proxy::new().inbound(srv).run();
                let client = $make_client(proxy.inbound, "disco.test.svc.cluster.local");
                let rsp = client.request(client.request_builder("/strip").header(REMOTE_IP_HEADER, FOO));

                assert_eq!(rsp.status(), 200);
            }

            #[test]
            fn outbound_should_set() {
                let _ = env_logger_init();
                let header = HeaderValue::from_static(BAR);

                let srv = $make_server().route("/set", "hello").run();
                let ctrl = controller::new().destination_and_close("disco.test.svc.cluster.local", srv.addr);
                let proxy = proxy::new().controller(ctrl.run()).outbound(srv).run();
                let client = $make_client(proxy.outbound, "disco.test.svc.cluster.local");
                let rsp = client.request(&mut client.request_builder("/set"));

                assert_eq!(rsp.status(), 200);
                assert_eq!(rsp.headers().get(REMOTE_IP_HEADER), Some(&header));
            }

            #[test]
            fn inbound_should_set() {
                let _ = env_logger_init();

                let header = HeaderValue::from_static(BAR);

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

}

mod http1 {
    use super::support::*;

    generate_tests! {
        server: server::http1, client: client::http1
    }
    generate_outbound_dns_limit_test! {
        server: server::http1, client: client::http1
    }

    mod absolute_uris {
        use super::super::support::*;

        generate_tests! {
            server: server::http1,
            client: client::http1_absolute_uris
        }

        generate_outbound_dns_limit_test! {
            server: server::http1,
            client: client::http1_absolute_uris
        }
    }

}

#[test]
fn outbound_updates_newer_services() {
    let _ = env_logger_init();

    let srv = server::http1().route("/h1", "hello h1").run();

    let ctrl = controller::new().destination_and_close("disco.test.svc.cluster.local", srv.addr);

    let proxy = proxy::new().controller(ctrl.run()).outbound(srv).run();

    // the HTTP2 service starts watching first, receiving an addr
    // from the controller
    let client1 = client::http2(proxy.outbound, "disco.test.svc.cluster.local");

    // Depending on the version of `hyper` we're using, protocol upgrades may or
    // may not be supported yet, so this may have a response status of either 200
    // or 500. Ignore the status code for now, and just expect that making the
    // request doesn't *error* (which `client.request` does for us).
    let _res = client1.request(&mut client1.request_builder("/h1"));
    // assert_eq!(res.status(), 200);

    // a new HTTP1 service needs to be build now, while the HTTP2
    // service already exists, so make sure previously sent addrs
    // get into the newer service
    let client2 = client::http1(proxy.outbound, "disco.test.svc.cluster.local");
    assert_eq!(client2.get("/h1"), "hello h1");
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
            let id = "foo.deployment.ns1.linkerd-managed.linkerd.svc.cluster.local";

            let srv = $make_server()
                .route_fn("/hallo", move |req| {
                    assert_eq!(req.headers()["l5d-client-id"], id);
                    Response::default()
                })
                .run();

            let in_proxy = proxy::new().inbound(srv).run_with_test_env(tls_env());

            let ctrl = controller::new();
            let dst = ctrl.destination_tx("disco.test.svc.cluster.local");
            dst.send(controller::destination_add_tls(
                in_proxy.inbound,
                id,
                "linkerd",
            ));

            let out_proxy = proxy::new()
                .controller(ctrl.run())
                .run_with_test_env(tls_env());

            let client = $make_client(out_proxy.outbound, "disco.test.svc.cluster.local");

            let res = client.request(&mut client.request_builder("/hallo"));
            assert_eq!(res.status(), 200);
            assert_eq!(res.headers()["l5d-server-id"], id);
        };
    }

    #[test]
    fn outbound_http1_l5d_server_id_l5d_client_id() {
        generate_l5d_tls_id_test! {
            server: server::http1,
            client: client::http1
        }
    }

    #[test]
    fn outbound_http2_l5d_server_id_l5d_client_id() {
        generate_l5d_tls_id_test! {
            server: server::http2,
            client: client::http2
        }
    }

    fn tls_env() -> app::config::TestEnv {
        use std::path::PathBuf;

        let (cert, key, trust_anchors) = {
            let path_to_string = |path: &PathBuf| {
                path.as_path()
                    .to_owned()
                    .into_os_string()
                    .into_string()
                    .unwrap()
            };
            let mut tls = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            tls.push("src");
            tls.push("transport");
            tls.push("tls");
            tls.push("testdata");

            tls.push("foo-ns1-ca1.crt");
            let cert = path_to_string(&tls);

            tls.set_file_name("foo-ns1-ca1.p8");
            let key = path_to_string(&tls);

            tls.set_file_name("ca1.pem");
            let trust_anchors = path_to_string(&tls);
            (cert, key, trust_anchors)
        };

        let mut env = app::config::TestEnv::new();

        env.put(app::config::ENV_TLS_CERT, cert);
        env.put(app::config::ENV_TLS_PRIVATE_KEY, key);
        env.put(app::config::ENV_TLS_TRUST_ANCHORS, trust_anchors);
        env.put(
            app::config::ENV_TLS_POD_IDENTITY,
            "foo.deployment.ns1.linkerd-managed.linkerd.svc.cluster.local".to_string(),
        );
        env.put(app::config::ENV_CONTROLLER_NAMESPACE, "linkerd".to_string());
        env.put(app::config::ENV_POD_NAMESPACE, "ns1".to_string());

        env
    }
}
