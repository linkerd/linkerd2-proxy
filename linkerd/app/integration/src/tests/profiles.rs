use crate::*;
use std::sync::atomic::{AtomicUsize, Ordering};

macro_rules! profile_test {
    (routes: [$($route:expr),+], budget: $budget:expr, with_client: $with_client:expr) => {
        profile_test! {
            routes: [$($route),+],
            budget: $budget,
            with_client: $with_client,
            with_metrics: |_m, _| async {}
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
        let _trace = trace_init();

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
            .route_async("/0.5",  move |req| {
                let fail = counter.fetch_add(1, Ordering::Relaxed) % 2 == 0;
                async move {
                    // Read the entire body before responding, so that the
                    // client doesn't fail when writing it out.
                    let _body = hyper::body::aggregate(req.into_body()).await;
                    Ok::<_, Error>(if fail {
                        Response::builder()
                            .status(533)
                            .body("nope".into())
                            .unwrap()
                    } else {
                        Response::builder()
                            .status(200)
                            .body("retried".into())
                            .unwrap()
                    })
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
            .run().await;
        let port = srv.addr.port();
        let ctrl = controller::new();

        let dst_tx = ctrl.destination_tx(&format!("{}:{}", host, port));
        dst_tx.send_addr(srv.addr);

        let profile_tx = ctrl.profile_tx(srv.addr.to_string());
        let routes = vec![
            // This route is used to get the proxy to start fetching the`
            // ServiceProfile. We'll keep GETting this route and checking
            // the metrics for the labels, to know that the other route
            // rules are now in place and the test can proceed.
            controller::route()
                .request_path("/load-profile")
                .label("load_profile", "test"),
            $($route,),+
        ];
        profile_tx.send(controller::profile(routes, $budget, vec![], host));

        let ctrl = ctrl.run().await;
        let proxy = proxy::new()
            .controller(ctrl)
            .outbound(srv)
            .run()
            .await;

        let client = client::$http(proxy.outbound, host);

        let metrics = client::http1(proxy.metrics, "localhost");

        // Poll metrics until we recognize the profile is loaded...
        loop {
            assert_eq!(client.get("/load-profile").await, "");
            let m = metrics.get("/metrics").await;
            if m.contains("rt_load_profile=\"test\"") {
                break;
            }

            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        $with_client(client).await;

        $with_metrics(metrics, port).await;
    }
}

#[tokio::test]
async fn retry_if_profile_allows() {
    profile_test! {
        routes: [
            controller::route()
                .request_any()
                // use default classifier
                .retryable(true)
        ],
        budget: Some(controller::retry_budget(Duration::from_secs(10), 0.1, 1)),
        with_client: |client: client::Client| async move {
            assert_eq!(client.get("/0.5").await, "retried");
        }
    }
}

#[tokio::test]
async fn retry_uses_budget() {
    profile_test! {
        routes: [
            controller::route()
                .request_any()
                .response_failure(500..600)
                .retryable(true)
        ],
        budget: Some(controller::retry_budget(Duration::from_secs(1), 0.1, 1)),
        with_client: |client: client::Client| async move {
            assert_eq!(client.get("/0.5").await, "retried");
            let res = client.request(client.request_builder("/0.5")).await.unwrap();
            assert_eq!(res.status(), 533);
        },
        with_metrics: |metrics: client::Client, port| async move {
            metrics::metric("route_retryable_total")
                .label("direction", "outbound")
                .label("dst", format_args!("profiles.test.svc.cluster.local:{}", port))
                .label("skipped", "no_budget")
                .value(1u64)
                .assert_in(&metrics)
                .await;
        }
    }
}

#[tokio::test]
async fn retry_with_small_post_bodies() {
    profile_test! {
        routes: [
            controller::route()
                .request_any()
                .response_failure(500..600)
                .retryable(true)
        ],
        budget: Some(controller::retry_budget(Duration::from_secs(10), 0.1, 1)),
        with_client: |client: client::Client| async move {
            let req = client.request_builder("/0.5")
                .method("POST")
                .body("req has a body".into())
                .unwrap();
            let res = client.request_body(req).await;
            assert_eq!(res.status(), 200);
        }
    }
}

#[tokio::test]
async fn does_not_retry_if_request_does_not_match() {
    profile_test! {
        routes: [
            controller::route()
                .request_path("/wont/match/anything")
                .response_failure(..)
                .retryable(true)
        ],
        budget: Some(controller::retry_budget(Duration::from_secs(10), 0.1, 1)),
        with_client: |client: client::Client| async move {
            let res = client.request(client.request_builder("/0.5")).await.unwrap();
            assert_eq!(res.status(), 533);
        }
    }
}

#[tokio::test]
async fn does_not_retry_if_earlier_response_class_is_success() {
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
        with_client: |client: client::Client| async move {
            let res = client.request(client.request_builder("/0.5")).await.unwrap();
            assert_eq!(res.status(), 533);
        }
    }
}

#[tokio::test]
async fn does_not_retry_with_body_if_not_post() {
    profile_test! {
        routes: [
            controller::route()
                .request_any()
                .response_failure(500..600)
                .retryable(true)
        ],
        budget: Some(controller::retry_budget(Duration::from_secs(10), 0.1, 1)),
        with_client: |client: client::Client| async move {
            let req = client.request_builder("/0.5")
                .method("PUT")
                .body("req has a body".into())
                .unwrap();
            let res = client.request_body(req).await;
            assert_eq!(res.status(), 533);
        }
    }
}

#[tokio::test]
async fn does_not_retry_if_body_is_too_long() {
    profile_test! {
        routes: [
            controller::route()
                .request_any()
                .response_failure(500..600)
                .retryable(true)
        ],
        budget: Some(controller::retry_budget(Duration::from_secs(10), 0.1, 1)),
        with_client: |client: client::Client| async move {
            let req = client.request_builder("/0.5")
                .method("POST")
                .body(hyper::Body::from(&[1u8; 64 * 1024 + 1][..]))
                .unwrap();
            let res = client.request_body(req).await;
            assert_eq!(res.status(), 533);
        }
    }
}

#[tokio::test]
async fn does_not_retry_if_body_lacks_known_length() {
    profile_test! {
        routes: [
            controller::route()
                .request_any()
                .response_failure(500..600)
                .retryable(true)
        ],
        budget: Some(controller::retry_budget(Duration::from_secs(10), 0.1, 1)),
        with_client: |client: client::Client| async move {
            let (mut tx, body) = hyper::body::Body::channel();
            let req = client.request_builder("/0.5")
                .method("POST")
                .body(body)
                .unwrap();
            let res = tokio::spawn(async move { client.request_body(req).await });
            let _ = tx.send_data(Bytes::from_static(b"hello"));
            let _ = tx.send_data(Bytes::from_static(b"world"));
            drop(tx);
            let res = res.await.unwrap();
            assert_eq!(res.status(), 533);
        }
    }
}

#[tokio::test]
async fn does_not_retry_if_missing_retry_budget() {
    profile_test! {
        routes: [
            controller::route()
                .request_any()
                .response_failure(500..600)
                .retryable(true)
        ],
        budget: None,
        with_client: |client: client::Client|async move {
            let res = client.request(client.request_builder("/0.5")).await.unwrap();
            assert_eq!(res.status(), 533);
        }
    }
}

#[tokio::test]
async fn ignores_invalid_retry_budget_ttl() {
    profile_test! {
        routes: [
            controller::route()
                .request_any()
                .response_failure(500..600)
                .retryable(true)
        ],
        budget: Some(controller::retry_budget(Duration::from_secs(1000), 0.1, 1)),
        with_client: |client: client::Client| async move {
            let res = client.request(client.request_builder("/0.5")).await.unwrap();
            assert_eq!(res.status(), 533);
        }
    }
}

#[tokio::test]
async fn ignores_invalid_retry_budget_ratio() {
    profile_test! {
        routes: [
            controller::route()
                .request_any()
                .response_failure(500..600)
                .retryable(true)
        ],
        budget: Some(controller::retry_budget(Duration::from_secs(10), 10_000.0, 1)),
        with_client: |client: client::Client| async move {
            let res = client.request(client.request_builder("/0.5")).await.unwrap();
            assert_eq!(res.status(), 533);
        }
    }
}

#[tokio::test]
async fn ignores_invalid_retry_budget_negative_ratio() {
    profile_test! {
        routes: [
            controller::route()
                .request_any()
                .response_failure(500..600)
                .retryable(true)
        ],
        budget: Some(controller::retry_budget(Duration::from_secs(10), -1.0, 1)),
        with_client: |client: client::Client| async move {
            let res = client.request(client.request_builder("/0.5")).await.unwrap();
            assert_eq!(res.status(), 533);
        }
    }
}

#[tokio::test]
async fn http2_failures_dont_leak_connection_window() {
    profile_test! {
        http: http2,
        routes: [
            controller::route()
                .request_any()
                .response_failure(500..600)
                .retryable(true)
        ],
        budget: Some(controller::retry_budget(Duration::from_secs(10), 1.0, 10)),
        with_client: |client: client::Client| async move {
            // Before https://github.com/carllerche/h2/pull/334, this would
            // hang since the retried failure would have leaked the 100k window
            // capacity, preventing the successful response from being read.
            assert_eq!(client.get("/0.5/100KB").await, "retried");
        },
        with_metrics: |_m, _| async {}
    }
}

#[tokio::test]
async fn timeout() {
    profile_test! {
        routes: [
            controller::route()
                .request_any()
                .timeout(Duration::from_millis(100))
        ],
        budget: None,
        with_client: |client: client::Client| async move {
            let res = client.request(client.request_builder("/1.0/sleep")).await.unwrap();
            assert_eq!(res.status(), 504);
        },
        with_metrics: |metrics: client::Client, port| async move {
            metrics::metric("route_response_total")
                .label("direction", "outbound")
                .label("dst", format_args!("profiles.test.svc.cluster.local:{}", port))
                .label("classification", "failure")
                .label("error", "timeout")
                .value(1u64)
                .assert_in(&metrics)
                .await;
        }
    }
}
