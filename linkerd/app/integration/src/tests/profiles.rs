use crate::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct TestBuilder {
    server: server::Server,
    routes: Vec<controller::RouteBuilder>,
    budget: Option<controller::pb::RetryBudget>,
    default_routes: bool,
}

struct Test {
    metrics: client::Client,
    client: client::Client,
    _proxy: proxy::Listening,
    port: u16,
    // stuff that tests currently never use, but we can't drop while the test is running
    _guards: (
        controller::DstSender,
        controller::ProfileSender,
        tracing::dispatcher::DefaultGuard,
    ),
}

impl TestBuilder {
    fn new(server: server::Server) -> Self {
        Self {
            server,
            routes: vec![
                // This route is used to get the proxy to start fetching the`
                // ServiceProfile. We'll keep GETting this route and checking
                // the metrics for the labels, to know that the other route
                // rules are now in place and the test can proceed.
                controller::route()
                    .request_path("/load-profile")
                    .label("load_profile", "test"),
            ],
            budget: Some(controller::retry_budget(Duration::from_secs(1), 0.1, 1)),
            default_routes: true,
        }
    }

    fn with_budget(self, budget: impl Into<Option<controller::pb::RetryBudget>>) -> Self {
        Self {
            budget: budget.into(),
            ..self
        }
    }

    fn with_profile_route(self, route: controller::RouteBuilder) -> Self {
        let mut routes = self.routes;
        routes.push(route);
        Self { routes, ..self }
    }

    async fn run(self) -> Test {
        let (trace, _) = trace_init();
        let host = "profiles.test.svc.cluster.local";
        let mut srv = self
            .server
            // This route is just called by the test setup, to trigger the proxy
            // to start fetching the ServiceProfile.
            .route_fn("/load-profile", |_| {
                Response::builder().status(201).body("".into()).unwrap()
            });
        if self.default_routes {
            let counter = AtomicUsize::new(0);
            let counter2 = AtomicUsize::new(0);
            let counter3 = AtomicUsize::new(0);
            srv = srv
                .route_fn("/1.0/sleep", move |_req| {
                    ::std::thread::sleep(Duration::from_secs(1));
                    Response::builder()
                        .status(200)
                        .body("slept".into())
                        .unwrap()
                })
                .route_async("/0.5", move |req| {
                    let fail = counter.fetch_add(1, Ordering::Relaxed) % 2 == 0;
                    async move {
                        // Read the entire body before responding, so that the
                        // client doesn't fail when writing it out.
                        let _body = hyper::body::to_bytes(req.into_body()).await;
                        tracing::debug!(body = ?_body.as_ref().map(|body| body.len()), "recieved body");
                        Ok::<_, Error>(if fail {
                            Response::builder().status(533).body("nope".into()).unwrap()
                        } else {
                            Response::builder()
                                .status(200)
                                .body("retried".into())
                                .unwrap()
                        })
                    }
                })
                .route_fn("/0.5/sleep", move |_req| {
                    ::std::thread::sleep(Duration::from_secs(1));
                    if counter2.fetch_add(1, Ordering::Relaxed) % 2 == 0 {
                        Response::builder().status(533).body("nope".into()).unwrap()
                    } else {
                        Response::builder()
                            .status(200)
                            .body("retried".into())
                            .unwrap()
                    }
                })
                .route_fn("/0.5/100KB", move |_req| {
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
                });
        };

        let srv = srv.run().await;

        let port = srv.addr.port();
        let ctrl = controller::new();

        let dst_tx = ctrl.destination_tx(&format!("{}:{}", host, port));
        dst_tx.send_addr(srv.addr);

        let ctrl = controller::new();

        let dst_tx = ctrl.destination_tx(&format!("{}:{}", host, port));
        dst_tx.send_addr(srv.addr);

        let profile_tx = ctrl.profile_tx(srv.addr.to_string());
        profile_tx.send(controller::profile(self.routes, self.budget, vec![], host));

        let ctrl = ctrl.run().await;
        let proxy = proxy::new().controller(ctrl).outbound(srv).run().await;

        let client = proxy.outbound_http_client(host);

        let metrics = client::http1(proxy.admin, "localhost");

        // Poll metrics until we recognize the profile is loaded...
        loop {
            assert_eq!(client.get("/load-profile").await, "");
            let m = metrics.get("/metrics").await;
            if m.contains("rt_load_profile=\"test\"") {
                break;
            }

            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        Test {
            metrics,
            client,
            _proxy: proxy,
            port,
            _guards: (dst_tx, profile_tx, trace),
        }
    }
}

mod cross_version {
    use super::*;

    pub(super) async fn retry_if_profile_allows(version: server::Server) {
        let test = TestBuilder::new(version)
            .with_profile_route(
                controller::route()
                    .request_any()
                    // use default classifier
                    .retryable(true),
            )
            .run()
            .await;

        assert_eq!(test.client.get("/0.5").await, "retried");
    }

    pub(super) async fn retry_uses_budget(version: server::Server) {
        let test = TestBuilder::new(version)
            .with_profile_route(
                controller::route()
                    .request_any()
                    .response_failure(500..600)
                    .retryable(true),
            )
            .run()
            .await;
        let client = &test.client;

        assert_eq!(client.get("/0.5").await, "retried");
        let res = client
            .request(client.request_builder("/0.5"))
            .await
            .unwrap();
        assert_eq!(res.status(), 533);
        assert_eq!(client.get("/0.5").await, "retried");

        metrics::metric("route_retryable_total")
            .label("direction", "outbound")
            .label(
                "dst",
                format_args!("profiles.test.svc.cluster.local:{}", test.port),
            )
            .label("skipped", "no_budget")
            .value(1u64)
            .assert_in(&test.metrics)
            .await;
    }

    pub(super) async fn retry_with_small_post_body(version: server::Server) {
        let test = TestBuilder::new(version)
            .with_profile_route(
                controller::route()
                    .request_any()
                    .response_failure(500..600)
                    .retryable(true),
            )
            .run()
            .await;

        let client = &test.client;
        let req = client
            .request_builder("/0.5")
            .method(http::Method::POST)
            .body("req has a body".into())
            .unwrap();
        let res = client.request_body(req).await;
        assert_eq!(res.status(), 200);
    }

    pub(super) async fn retry_with_small_put_body(version: server::Server) {
        let test = TestBuilder::new(version)
            .with_profile_route(
                controller::route()
                    .request_any()
                    .response_failure(500..600)
                    .retryable(true),
            )
            .run()
            .await;

        let client = &test.client;
        let req = client
            .request_builder("/0.5")
            .method(http::Method::PUT)
            .body("req has a body".into())
            .unwrap();
        let res = client.request_body(req).await;
        assert_eq!(res.status(), 200);
    }

    pub(super) async fn retry_without_content_length(version: server::Server) {
        let test = TestBuilder::new(version)
            .with_profile_route(
                controller::route()
                    .request_any()
                    .response_failure(500..600)
                    .retryable(true),
            )
            .run()
            .await;

        let client = test.client;
        let (mut tx, body) = hyper::body::Body::channel();
        let req = client
            .request_builder("/0.5")
            .method("POST")
            .body(body)
            .unwrap();
        let res = tokio::spawn(async move { client.request_body(req).await });
        tx.send_data(Bytes::from_static(b"hello"))
            .await
            .expect("the whole body should be read");
        tx.send_data(Bytes::from_static(b"world"))
            .await
            .expect("the whole body should be read");
        drop(tx);
        let res = res.await.unwrap();
        assert_eq!(res.status(), 200);
    }

    pub(super) async fn does_not_retry_if_request_does_not_match(version: server::Server) {
        let test = TestBuilder::new(version)
            .with_profile_route(
                controller::route()
                    .request_path("/wont/match/anything")
                    .response_failure(..)
                    .retryable(true),
            )
            .run()
            .await;

        let client = &test.client;
        let res = client
            .request(client.request_builder("/0.5"))
            .await
            .unwrap();
        assert_eq!(res.status(), 533);
    }

    pub(super) async fn does_not_retry_if_earlier_response_class_is_success(
        version: server::Server,
    ) {
        let test = TestBuilder::new(version)
            .with_profile_route(
                controller::route()
                    .request_any()
                    // prevent 533s from being retried
                    .response_success(533..534)
                    .response_failure(500..600)
                    .retryable(true),
            )
            .run()
            .await;

        let client = &test.client;
        let res = client
            .request(client.request_builder("/0.5"))
            .await
            .unwrap();
        assert_eq!(res.status(), 533);
    }

    pub(super) async fn does_not_retry_if_body_is_too_long(version: server::Server) {
        let test = TestBuilder::new(version)
            .with_profile_route(
                controller::route()
                    .request_any()
                    // prevent 533s from being retried
                    .response_success(533..534)
                    .response_failure(500..600)
                    .retryable(true),
            )
            .run()
            .await;

        let client = &test.client;
        let req = client
            .request_builder("/0.5")
            .method("POST")
            .body(hyper::Body::from(&[1u8; 64 * 1024 + 1][..]))
            .unwrap();
        let res = client.request_body(req).await;
        assert_eq!(res.status(), 533);
    }

    pub(super) async fn does_not_retry_if_streaming_body_exceeds_max_length(
        version: server::Server,
    ) {
        // TODO(eliza): if we make the max length limit configurable, update this
        // test to test the configurable max length limit...
        let test = TestBuilder::new(version)
            .with_profile_route(
                controller::route()
                    .request_any()
                    .response_failure(500..600)
                    .retryable(true),
            )
            .run()
            .await;

        let client = test.client;
        let (mut tx, body) = hyper::body::Body::channel();
        let req = client
            .request_builder("/0.5")
            .method("POST")
            .body(body)
            .unwrap();
        let res = tokio::spawn(async move { client.request_body(req).await });
        // send a 32k chunk
        tx.send_data(Bytes::from(&[1u8; 32 * 1024][..]))
            .await
            .expect("the whole body should be read");
        // ...and another one...
        tx.send_data(Bytes::from(&[1u8; 32 * 1024][..]))
            .await
            .expect("the whole body should be read");
        // ...and a third one (exceeding the max length limit)
        tx.send_data(Bytes::from(&[1u8; 32 * 1024][..]))
            .await
            .expect("the whole body should be read");
        drop(tx);
        let res = res.await.unwrap();

        assert_eq!(res.status(), 533);
    }

    pub(super) async fn does_not_retry_if_missing_retry_budget(version: server::Server) {
        let test = TestBuilder::new(version)
            .with_profile_route(
                controller::route()
                    .request_any()
                    .response_failure(500..600)
                    .retryable(true),
            )
            .with_budget(None)
            .run()
            .await;

        let client = &test.client;
        let res = client
            .request(client.request_builder("/0.5"))
            .await
            .unwrap();
        assert_eq!(res.status(), 533);
    }

    pub(super) async fn ignores_invalid_retry_budget_ttl(version: server::Server) {
        let test = TestBuilder::new(version)
            .with_profile_route(
                controller::route()
                    .request_any()
                    .response_failure(500..600)
                    .retryable(true),
            )
            .with_budget(controller::retry_budget(Duration::from_secs(1000), 0.1, 1))
            .run()
            .await;

        let client = &test.client;
        let res = client
            .request(client.request_builder("/0.5"))
            .await
            .unwrap();
        assert_eq!(res.status(), 533);
    }

    pub(super) async fn ignores_invalid_retry_budget_ratio(version: server::Server) {
        let test = TestBuilder::new(version)
            .with_profile_route(
                controller::route()
                    .request_any()
                    .response_failure(500..600)
                    .retryable(true),
            )
            .with_budget(controller::retry_budget(
                Duration::from_secs(10),
                10_000.0,
                1,
            ))
            .run()
            .await;

        let client = &test.client;
        let res = client
            .request(client.request_builder("/0.5"))
            .await
            .unwrap();
        assert_eq!(res.status(), 533);
    }

    pub(super) async fn ignores_invalid_retry_budget_negative_ratio(version: server::Server) {
        let test = TestBuilder::new(version)
            .with_profile_route(
                controller::route()
                    .request_any()
                    .response_failure(500..600)
                    .retryable(true),
            )
            .with_budget(controller::retry_budget(Duration::from_secs(10), -1.0, 1))
            .run()
            .await;

        let client = &test.client;
        let res = client
            .request(client.request_builder("/0.5"))
            .await
            .unwrap();
        assert_eq!(res.status(), 533);
    }

    pub(super) async fn timeout(version: server::Server) {
        let test = TestBuilder::new(version)
            .with_profile_route(
                controller::route()
                    .request_any()
                    .timeout(Duration::from_millis(100)),
            )
            .with_budget(None)
            .run()
            .await;

        let client = &test.client;
        let res = client
            .request(client.request_builder("/1.0/sleep"))
            .await
            .unwrap();
        assert_eq!(res.status(), 504);

        metrics::metric("route_response_total")
            .label("direction", "outbound")
            .label(
                "dst",
                format_args!("profiles.test.svc.cluster.local:{}", test.port),
            )
            .label("classification", "failure")
            .label("error", "timeout")
            .value(1u64)
            .assert_in(&test.metrics)
            .await;
    }
}

macro_rules! cross_version {
    ($version:expr => $($test:ident),+ $(,)?) => {
        $(
            #[tokio::test]
            async fn $test() {
                cross_version::$test($version).await
            }
        )+
    };
}
mod http1 {
    use super::*;

    cross_version! {
        server::http1() =>
        retry_if_profile_allows,
        retry_uses_budget,
        retry_with_small_post_body,
        retry_with_small_put_body,
        retry_without_content_length,
        does_not_retry_if_request_does_not_match,
        does_not_retry_if_earlier_response_class_is_success,
        does_not_retry_if_body_is_too_long,
        does_not_retry_if_streaming_body_exceeds_max_length,
        does_not_retry_if_missing_retry_budget,
        ignores_invalid_retry_budget_ttl,
        ignores_invalid_retry_budget_ratio,
        ignores_invalid_retry_budget_negative_ratio,
        timeout,
    }
}

mod http2 {
    use super::*;

    cross_version! {
        server::http1() =>
        retry_if_profile_allows,
        retry_uses_budget,
        retry_with_small_post_body,
        retry_with_small_put_body,
        retry_without_content_length,
        does_not_retry_if_request_does_not_match,
        does_not_retry_if_earlier_response_class_is_success,
        does_not_retry_if_body_is_too_long,
        does_not_retry_if_streaming_body_exceeds_max_length,
        does_not_retry_if_missing_retry_budget,
        ignores_invalid_retry_budget_ttl,
        ignores_invalid_retry_budget_ratio,
        ignores_invalid_retry_budget_negative_ratio,
        timeout,
    }

    #[tokio::test]
    async fn http2_failures_dont_leak_connection_window() {
        let test = TestBuilder::new(server::http2())
            .with_profile_route(
                controller::route()
                    .request_any()
                    .response_failure(500..600)
                    .retryable(true),
            )
            .run()
            .await;

        // Before https://github.com/carllerche/h2/pull/334, this would
        // hang since the retried failure would have leaked the 100k window
        // capacity, preventing the successful response from being read.
        assert_eq!(test.client.get("/0.5/100KB").await, "retried")
    }
}
