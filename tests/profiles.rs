#![recursion_limit = "128"]
#![deny(warnings)]
mod support;
use self::support::*;

use std::sync::atomic::{AtomicUsize, Ordering};

macro_rules! profile_test {
    (routes: [$($route:expr),+], budget: $budget:expr, with_client: $with_client:expr) => {
        profile_test! {
            routes: [$($route),+],
            budget: $budget,
            with_client: $with_client,
            with_metrics: |_m| {}
        }
    };
    (routes: [$($route:expr),+], budget: $budget:expr, with_client: $with_client:expr, with_metrics: $with_metrics:expr) => {
        profile_test! {
            http: http1,
            routes: [$($route),+],
            budget: $budget,
            with_client: $with_client,
            with_metrics: $with_metrics
        }
    };
    (http: $http:ident, routes: [$($route:expr),+], budget: $budget:expr, with_client: $with_client:expr, with_metrics: $with_metrics:expr) => {
        let _ = env_logger_init();

        let counter = AtomicUsize::new(0);
        let counter2 = AtomicUsize::new(0);
        let counter3 = AtomicUsize::new(0);
        let host = "profiles.test.svc.cluster.local";

        let srv = server::$http()
            // This route is just called by the test setup, to trigger the proxy
            // to start fetching the ServiceProfile.
            .route_fn("/load-profile", |_| {
                Response::builder()
                    .status(201)
                    .body("".into())
                    .unwrap()
            })
            .route_fn("/1.0/sleep",  move |_req| {
                ::std::thread::sleep(Duration::from_secs(1));
                Response::builder()
                    .status(200)
                    .body("slept".into())
                    .unwrap()
            })
            .route_fn("/0.5",  move |_req| {
                if counter.fetch_add(1, Ordering::Relaxed) % 2 == 0 {
                    Response::builder()
                        .status(533)
                        .body("nope".into())
                        .unwrap()
                } else {
                    Response::builder()
                        .status(200)
                        .body("retried".into())
                        .unwrap()
                }
            })
            .route_fn("/0.5/sleep",  move |_req| {
                ::std::thread::sleep(Duration::from_secs(1));
                if counter2.fetch_add(1, Ordering::Relaxed) % 2 == 0 {
                    Response::builder()
                        .status(533)
                        .body("nope".into())
                        .unwrap()
                } else {
                    Response::builder()
                        .status(200)
                        .body("retried".into())
                        .unwrap()
                }
            })
            .route_fn("/0.5/100KB",  move |_req| {
                if counter3.fetch_add(1, Ordering::Relaxed) % 2 == 0 {
                    Response::builder()
                        .status(533)
                        .body(vec![b'x'; 1024 * 100].into())
                        .unwrap()
                } else {
                    Response::builder()
                        .status(200)
                        .body("retried".into())
                        .unwrap()
                }
            })
            .run();
        let ctrl = controller::new();

        let dst_tx = ctrl.destination_tx(host);
        dst_tx.send_addr(srv.addr);

        let profile_tx = ctrl.profile_tx(host);
        let routes = vec![
            // This route is used to get the proxy to start fetching the
            // ServiceProfile. We'll keep GETting this route and checking
            // the metrics for the labels, to know that the other route
            // rules are now in place and the test can proceed.
            controller::route()
                .request_path("/load-profile")
                .label("load_profile", "test"),
            $($route,),+
        ];
        profile_tx.send(controller::profile(routes, $budget, vec![]));

        let ctrl = ctrl.run();
        let proxy = proxy::new()
            .controller(ctrl)
            .outbound(srv)
            .run();

        let client = client::$http(proxy.outbound, host);

        let metrics = client::http1(proxy.metrics, "localhost");

        // Poll metrics until we recognize the profile is loaded...
        loop {
            assert_eq!(client.get("/load-profile"), "");
            let m = metrics.get("/metrics");
            if m.contains("rt_load_profile=\"test\"") {
                break;
            }

            ::std::thread::sleep(::std::time::Duration::from_millis(200));
        }

        $with_client(client);

        $with_metrics(metrics);
    }
}

#[test]
fn retry_if_profile_allows() {
    profile_test! {
        routes: [
            controller::route()
                .request_any()
                // use default classifier
                .retryable(true)
        ],
        budget: Some(controller::retry_budget(Duration::from_secs(10), 0.1, 1)),
        with_client: |client: client::Client| {
            assert_eq!(client.get("/0.5"), "retried");
        }
    }
}

#[test]
fn retry_uses_budget() {
    profile_test! {
        routes: [
            controller::route()
                .request_any()
                .response_failure(500..600)
                .retryable(true)
        ],
        budget: Some(controller::retry_budget(Duration::from_secs(1), 0.1, 1)),
        with_client: |client: client::Client| {
            assert_eq!(client.get("/0.5"), "retried");
            let res = client.request(&mut client.request_builder("/0.5"));
            assert_eq!(res.status(), 533);
        },
        with_metrics: |metrics: client::Client| {
            assert_eventually_contains!(
                metrics.get("/metrics"),
                "route_actual_retry_skipped_total{direction=\"outbound\",dst=\"profiles.test.svc.cluster.local:80\",skipped=\"budget\"} 1"
            );
        }
    }
}

#[test]
fn does_not_retry_if_request_does_not_match() {
    profile_test! {
        routes: [
            controller::route()
                .request_path("/wont/match/anything")
                .response_failure(..)
                .retryable(true)
        ],
        budget: Some(controller::retry_budget(Duration::from_secs(10), 0.1, 1)),
        with_client: |client: client::Client| {
            let res = client.request(&mut client.request_builder("/0.5"));
            assert_eq!(res.status(), 533);
        }
    }
}

#[test]
fn does_not_retry_if_earlier_response_class_is_success() {
    profile_test! {
        routes: [
            controller::route()
                .request_any()
                // prevent 533s from being retried
                .response_success(533..534)
                .response_failure(500..600)
                .retryable(true)
        ],
        budget: Some(controller::retry_budget(Duration::from_secs(10), 0.1, 1)),
        with_client: |client: client::Client| {
            let res = client.request(&mut client.request_builder("/0.5"));
            assert_eq!(res.status(), 533);
        }
    }
}

#[test]
fn does_not_retry_if_request_has_body() {
    profile_test! {
        routes: [
            controller::route()
                .request_any()
                .response_failure(500..600)
                .retryable(true)
        ],
        budget: Some(controller::retry_budget(Duration::from_secs(10), 0.1, 1)),
        with_client: |client: client::Client| {
            let req = client.request_builder("/0.5")
                .method("POST")
                .body("req has a body".into())
                .unwrap();
            let res = client.request_body(req);
            assert_eq!(res.status(), 533);
        }
    }
}

#[test]
fn does_not_retry_if_missing_retry_budget() {
    profile_test! {
        routes: [
            controller::route()
                .request_any()
                .response_failure(500..600)
                .retryable(true)
        ],
        budget: None,
        with_client: |client: client::Client| {
            let res = client.request(&mut client.request_builder("/0.5"));
            assert_eq!(res.status(), 533);
        }
    }
}

#[test]
fn ignores_invalid_retry_budget_ttl() {
    profile_test! {
        routes: [
            controller::route()
                .request_any()
                .response_failure(500..600)
                .retryable(true)
        ],
        budget: Some(controller::retry_budget(Duration::from_secs(1000), 0.1, 1)),
        with_client: |client: client::Client| {
            let res = client.request(&mut client.request_builder("/0.5"));
            assert_eq!(res.status(), 533);
        }
    }
}

#[test]
fn ignores_invalid_retry_budget_ratio() {
    profile_test! {
        routes: [
            controller::route()
                .request_any()
                .response_failure(500..600)
                .retryable(true)
        ],
        budget: Some(controller::retry_budget(Duration::from_secs(10), 10_000.0, 1)),
        with_client: |client: client::Client| {
            let res = client.request(&mut client.request_builder("/0.5"));
            assert_eq!(res.status(), 533);
        }
    }
}

#[test]
fn ignores_invalid_retry_budget_negative_ratio() {
    profile_test! {
        routes: [
            controller::route()
                .request_any()
                .response_failure(500..600)
                .retryable(true)
        ],
        budget: Some(controller::retry_budget(Duration::from_secs(10), -1.0, 1)),
        with_client: |client: client::Client| {
            let res = client.request(&mut client.request_builder("/0.5"));
            assert_eq!(res.status(), 533);
        }
    }
}

#[test]
fn http2_failures_dont_leak_connection_window() {
    profile_test! {
        http: http2,
        routes: [
            controller::route()
                .request_any()
                .response_failure(500..600)
                .retryable(true)
        ],
        budget: Some(controller::retry_budget(Duration::from_secs(10), 1.0, 10)),
        with_client: |client: client::Client| {
            // Before https://github.com/carllerche/h2/pull/334, this would
            // hang since the retried failure would have leaked the 100k window
            // capacity, preventing the successful response from being read.
            assert_eq!(client.get("/0.5/100KB"), "retried");
        },
        with_metrics: |_m| {}
    }
}

#[test]
fn timeout() {
    profile_test! {
        routes: [
            controller::route()
                .request_any()
                .timeout(Duration::from_millis(100))
        ],
        budget: None,
        with_client: |client: client::Client| {
            let res = client.request(&mut client.request_builder("/1.0/sleep"));
            assert_eq!(res.status(), 504);
        },
        with_metrics: |metrics: client::Client| {
            assert_eventually_contains!(
                metrics.get("/metrics"),
                "route_response_total{direction=\"outbound\",dst=\"profiles.test.svc.cluster.local:80\",status_code=\"504\",classification=\"failure\",error=\"timeout\"} 1"
            );
        }
    }
}

#[test]
fn traffic_split() {
    let _ = env_logger_init();
    let apex = "profiles.test.svc.cluster.local";
    let leaf_a = "a.profiles.test.svc.cluster.local";
    let leaf_b = "b.profiles.test.svc.cluster.local";

    let apex_responses = Arc::new(AtomicUsize::new(0));
    let leaf_a_responses = Arc::new(AtomicUsize::new(0));
    let leaf_b_responses = Arc::new(AtomicUsize::new(0));

    let a_rsp = apex_responses.clone();
    let apex_srv = server::http1()
        .route_fn("/load-profile", |_| {
            Response::builder().status(201).body("".into()).unwrap()
        })
        .route_fn("/traffic-split", move |_req| {
            a_rsp.fetch_add(1, Ordering::SeqCst);
            Response::builder().status(200).body("".into()).unwrap()
        })
        .run();

    let la_rsp = leaf_a_responses.clone();
    let leaf_a_srv = server::http1()
        .route_fn("/load-profile", |_| {
            Response::builder().status(201).body("".into()).unwrap()
        })
        .route_fn("/traffic-split", move |_req| {
            la_rsp.fetch_add(1, Ordering::SeqCst);
            Response::builder()
                .status(200)
                .body("leaf-a".into())
                .unwrap()
        })
        .run();

    let lb_rsp = leaf_b_responses.clone();
    let leaf_b_srv = server::http1()
        .route_fn("/load-profile", |_| {
            Response::builder().status(201).body("".into()).unwrap()
        })
        .route_fn("/traffic-split", move |_req| {
            lb_rsp.fetch_add(1, Ordering::SeqCst);
            Response::builder()
                .status(200)
                .body("leaf-b".into())
                .unwrap()
        })
        .run();

    let ctrl = controller::new_unordered();

    let leaf_a_authority = format!("{}:{}", leaf_a, leaf_a_srv.addr.port());
    let leaf_b_authority = format!("{}:{}", leaf_b, leaf_b_srv.addr.port());

    let apex_dst_send = ctrl.destination_tx(apex);
    let leaf_a_dst_send = ctrl.destination_tx(leaf_a_authority.as_str());
    let leaf_b_dst_send = ctrl.destination_tx(leaf_b_authority.as_str());

    apex_dst_send.send_addr(apex_srv.addr);
    leaf_a_dst_send.send_addr(leaf_a_srv.addr);
    leaf_b_dst_send.send_addr(leaf_b_srv.addr);

    let profile_send = ctrl.profile_tx(apex);

    let ctrl = ctrl.run();
    let proxy = proxy::new().controller(ctrl).run();

    let client = client::http1(proxy.outbound, apex);
    let apex_metrics = client::http1(proxy.metrics, "localhost");

    let n = 100;

    // 1. Send `n` requests to apex service
    for _ in 0..n {
        assert_eq!(client.get("/traffic-split"), "");
    }

    // 2. Apex service should send `n` responses
    assert_eq!(apex_responses.load(Ordering::SeqCst), n);

    // 3. Leaf services should send 0 responses
    assert_eq!(leaf_a_responses.load(Ordering::SeqCst), 0);
    assert_eq!(leaf_b_responses.load(Ordering::SeqCst), 0);

    // 4. Load service profile that defines traffic split on Apex
    profile_send.send(controller::profile(
        vec![
            controller::route()
                .request_path("/load-profile")
                .label("load_profile", "test"),
            controller::route().request_any(),
        ],
        None,
        vec![
            controller::traffic_split(leaf_a_authority, 5000),
            controller::traffic_split(leaf_b_authority, 5000),
        ],
    ));

    // 5. Poll metrics until we recognize the profile is loaded
    loop {
        assert_eq!(client.get("/load-profile"), "");
        let m = apex_metrics.get("/metrics");
        if m.contains("rt_load_profile=\"test\"") {
            break;
        }

        ::std::thread::sleep(::std::time::Duration::from_millis(200));
    }
    apex_responses.store(0, Ordering::SeqCst);

    // 6. Send `n` requests to apex service
    for _ in 0..n {
        let rsp = client.get("/traffic-split");
        assert!(rsp == "leaf-a" || rsp == "leaf-b");
    }

    // 7. Apex proxy metrics should assert there are >0 responses
    assert!(leaf_a_responses.load(Ordering::SeqCst) > 0);
    assert!(leaf_b_responses.load(Ordering::SeqCst) > 0);
    assert_eq!(apex_responses.load(Ordering::SeqCst), 0);
}
