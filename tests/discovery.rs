#![deny(warnings)]
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
            let _ = env_logger::try_init();
            let srv = $make_server().route("/", "hello").run();
            let srv_addr = srv.addr;

            let mut env = app::config::TestEnv::new();
            env.put(app::config::ENV_DESTINATION_CLIENT_CONCURRENCY_LIMIT, "2".to_owned());

            let ctrl = controller::new();
            let _txs = (1..=3).map(|n| {
                let disco_n = format!("disco{}.test.svc.cluster.local", n);
                let tx = ctrl.destination_tx(&disco_n);
                tx.send_addr(srv_addr);
                tx // This will go into a vec, to keep the stream open.
            }).collect::<Vec<_>>();

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
            let rsp = client.request(req.method("GET"));
            // The request should fail.
            assert_eq!(rsp.status(), http::StatusCode::INTERNAL_SERVER_ERROR);

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
    }
}

macro_rules! generate_tests {
    (server: $make_server:path, client: $make_client:path) => {
        use linkerd2_proxy_api as pb;

        #[test]
        fn outbound_asks_controller_api() {
            let _ = env_logger::try_init();
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
            let _ = env_logger::try_init();
            let srv = $make_server().route("/", "hello").run();
            let srv_addr = srv.addr;

            let mut env = app::config::TestEnv::new();

            // Set the maximum number of Destination service resolutions
            // lower than the total router capacity, so we can reach the
            // maximum number of destinations before we start evicting
            // routes.
            env.put(app::config::ENV_DESTINATION_CLIENT_CONCURRENCY_LIMIT, "10".to_owned());
            env.put(app::config::ENV_OUTBOUND_ROUTER_CAPACITY, "11".to_owned());
            env.put(app::config::ENV_OUTBOUND_ROUTER_MAX_IDLE_AGE, "1s".to_owned());

            let ctrl = controller::new();
            let _txs = (1..=11).map(|n| {
                let disco_n = format!("disco{}.test.svc.cluster.local", n);
                let tx = ctrl.destination_tx(&disco_n);
                tx.send_addr(srv_addr);
                tx // This will go into a vec, to keep the stream open.
            }).collect::<Vec<_>>();

            let proxy = proxy::new()
                .controller(ctrl.run())
                .outbound(srv)
                .run_with_test_env(env);

            // Make ten requests that go through service discovery, to reach the
            // maximum number of Destination resolutions.
            for n in 1..=10 {
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
            let client = $make_client(proxy.outbound, "disco11.test.svc.cluster.local");
            println!("trying 11th destination...");
            let mut req = client.request_builder("/");
            let rsp = client.request(req.method("GET"));
            // The request should fail.
            assert_eq!(rsp.status(), http::StatusCode::INTERNAL_SERVER_ERROR);

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
            println!("trying 11th destination again...");
            let client = $make_client(proxy.outbound, "disco11.test.svc.cluster.local");
            assert_eq!(client.get("/"), "hello");
        }

        #[test]
        fn outbound_reconnects_if_controller_stream_ends() {
            let _ = env_logger::try_init();

            let srv = $make_server().route("/recon", "nect").run();

            let ctrl = controller::new()
                .destination_close("disco.test.svc.cluster.local")
                .destination_and_close("disco.test.svc.cluster.local", srv.addr);

            let proxy = proxy::new().controller(ctrl.run()).outbound(srv).run();
            let client = $make_client(proxy.outbound, "disco.test.svc.cluster.local");

            assert_eq!(client.get("/recon"), "nect");
        }

        #[test]
        #[cfg_attr(not(feature = "flaky_tests"), ignore)]
        fn outbound_destinations_reset_on_reconnect_followed_by_no_endpoints_exists() {
            outbound_destinations_reset_on_reconnect(move || {
                Some(controller::destination_exists_with_no_endpoints())
            })
        }

        #[test]
        #[cfg_attr(not(feature = "flaky_tests"), ignore)]
        fn outbound_destinations_reset_on_reconnect_followed_by_add_none() {
            outbound_destinations_reset_on_reconnect(move || {
                Some(controller::destination_add_none())
            })
        }

        #[test]
        #[cfg_attr(not(feature = "flaky_tests"), ignore)]
        fn outbound_destinations_reset_on_reconnect_followed_by_remove_none() {
            outbound_destinations_reset_on_reconnect(move || {
                Some(controller::destination_remove_none())
            })
        }

        fn outbound_destinations_reset_on_reconnect<F>(f: F)
            where F: Fn() -> Option<pb::destination::Update> + Send + 'static
        {
            use std::thread;
            let _ = env_logger::try_init();
            let mut env = app::config::TestEnv::new();

            // set the bind timeout to 100 ms.
            env.put(app::config::ENV_BIND_TIMEOUT, "100ms".to_owned());

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
            match f() {
                None => drop(dst_tx1),
                Some(up) => dst_tx1.send(up),
            }

            // Wait for the reconnect to happen. TODO: Replace this flaky logic.
            thread::sleep(Duration::from_millis(1000));

            // This will time out since there are no endpoints.
            let mut req = initially_exists.request_builder("/");
            let rsp = initially_exists.request(req.method("GET"));
            // the request should time out
            assert_eq!(rsp.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
        }

        #[test]
        #[cfg_attr(not(feature = "flaky_tests"), ignore)]
        fn outbound_times_out() {
            let _ = env_logger::try_init();
            let mut env = app::config::TestEnv::new();

            // set the bind timeout to 100 ms.
            env.put(app::config::ENV_BIND_TIMEOUT, "100ms".to_owned());

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
            let _ = env_logger::try_init();

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
            let _ = env_logger::try_init();

            let srv = $make_server()
                .route("/", "hello")
                .run();

            // Used to delay `listen` in the server, to force connection refused errors.
            let (tx, rx) = oneshot::channel();

            let ctrl = controller::new();

            let dst_tx = ctrl.destination_tx("disco.test.svc.cluster.local");
            dst_tx.send_addr(srv.addr);
            // but don't drop, to not trigger stream closing reconnects

            let mut env = app::config::TestEnv::new();

            // set the backoff timeout to 100 ms.
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
    let _ = env_logger::try_init();

    let srv = server::http1().route("/h1", "hello h1").run();

    let ctrl = controller::new()
        .destination_and_close("disco.test.svc.cluster.local", srv.addr);

    let proxy = proxy::new()
        .controller(ctrl.run())
        .outbound(srv)
        .run();

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
        let _ = env_logger::try_init();

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

        let proxy = proxy::new()
            .controller(ctrl.run())
            .run();

        let client = client::http1(proxy.outbound, "disco.test.svc.cluster.local");

        let res = client.request(&mut client.request_builder("/hint"));
        assert_eq!(res.status(), 200);
        assert_eq!(res.version(), http::Version::HTTP_11);
    }


    #[test]
    fn inbound_http1() {
        let _ = env_logger::try_init();

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

        let proxy = proxy::new()
            .controller(ctrl.run())
            .inbound(srv)
            .run();

        // This client will be used as a mocked-other-proxy.
        let client = client::http2(proxy.inbound, "disco.test.svc.cluster.local");

        let res = client.request(
            client
                .request_builder("/h1")
                .header("l5d-orig-proto", "HTTP/1.1")
        );
        assert_eq!(res.status(), 200);
        assert_eq!(res.version(), http::Version::HTTP_2);
    }
}
