#![recursion_limit="128"]
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
            .route_fn("/load-profile", |_| {
                Response::builder()
                    .status(201)
                    .body("".into())
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
        let routes = vec![$(
            $route,
        ),+];
        profile_tx.send(controller::profile(routes, $budget));

        let ctrl = ctrl.run();
        let proxy = proxy::new()
            .controller(ctrl)
            .outbound(srv)
            .run();

        let client = client::$http(proxy.outbound, host);
        assert_eq!(client.get("/load-profile"), "");

        $with_client(client);

        let metrics = client::http1(proxy.metrics, "localhost");
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
