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

    /// Don't add the default routes to the test server.
    ///
    /// Currently, no test actually *requires* this, but we may as well not add
    /// them when they won't be used...
    fn no_default_routes(self) -> Self {
        Self {
            default_routes: false,
            ..self
        }
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
            srv = Self::add_default_routes(srv);
        };

        let srv = srv.run().await;

        let port = srv.addr.port();
        let ctrl = controller::new();

        let dst_tx = ctrl.destination_tx(format!("{}:{}", host, port));
        dst_tx.send_addr(srv.addr);

        let ctrl = controller::new();

        let dst_tx = ctrl.destination_tx(format!("{}:{}", host, port));
        dst_tx.send_addr(srv.addr);

        let profile_tx = ctrl.profile_tx(srv.addr.to_string());
        profile_tx.send(controller::profile(self.routes, self.budget, vec![], host));

        let ctrl = ctrl.run().await;
        let proxy = proxy::new().controller(ctrl).outbound(srv).run().await;

        let client = proxy.outbound_http_client(host);

        let metrics = client::http1(proxy.admin, "localhost");

        Self::load_profile(&client, &metrics).await;

        Test {
            metrics,
            client,
            _proxy: proxy,
            port,
            _guards: (dst_tx, profile_tx, trace),
        }
    }

    fn add_default_routes(srv: server::Server) -> server::Server {
        let counter = AtomicUsize::new(0);
        let counter2 = AtomicUsize::new(0);
        let counter3 = AtomicUsize::new(0);

        srv.route_fn("/1.0/sleep", move |_req| {
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
        })
    }

    async fn load_profile(client: &client::Client, metrics: &client::Client) {
        // Poll metrics until we recognize the profile is loaded...
        loop {
            assert_eq!(client.get("/load-profile").await, "");
            let m = metrics.get("/metrics").await;
            if m.contains("rt_load_profile=\"test\"") {
                break;
            }

            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
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

mod http1 {
    use super::*;

    version_tests! {
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

    version_tests! {
        server::http2() =>
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

mod grpc_retry {
    use super::*;
    use http::header::{HeaderName, HeaderValue};
    static GRPC_STATUS: HeaderName = HeaderName::from_static("grpc-status");
    static GRPC_STATUS_OK: HeaderValue = HeaderValue::from_static("0");
    static GRPC_STATUS_UNAVAILABLE: HeaderValue = HeaderValue::from_static("14");

    /// Tests that a gRPC request is retried when a `grpc-status` code other
    /// than `OK` is sent in the response headers.
    #[tokio::test]
    async fn retries_failure_in_headers() {
        let retries = Arc::new(AtomicUsize::new(0));
        let srv = server::http2().route_fn("/retry", {
            let retries = retries.clone();
            move |_| {
                let success = retries.fetch_add(1, Ordering::Relaxed) > 0;
                let header = if success {
                    GRPC_STATUS_OK.clone()
                } else {
                    GRPC_STATUS_UNAVAILABLE.clone()
                };
                let rsp = Response::builder()
                    .header(GRPC_STATUS.clone(), header)
                    .status(200)
                    .body(hyper::Body::empty())
                    .unwrap();
                tracing::debug!(headers = ?rsp.headers());
                rsp
            }
        });

        let test = TestBuilder::new(srv)
            .with_profile_route(controller::route().request_path("/retry").retryable(true))
            .no_default_routes()
            .run()
            .await;
        let client = &test.client;

        let req = client
            .request_builder("/retry")
            .header(http::header::CONTENT_TYPE, "application/grpc");
        let res = client.request(req).await.unwrap();

        assert_eq!(res.status(), 200);
        assert_eq!(res.headers().get(&GRPC_STATUS), Some(&GRPC_STATUS_OK));

        assert_eq!(retries.load(Ordering::Relaxed), 2);
    }

    /// Tests that a gRPC request is retried when a `grpc-status` code other
    /// than `OK` is sent in the response's trailers.
    ///
    /// Reproduces https://github.com/linkerd/linkerd2/issues/7701.
    #[tokio::test]
    async fn retries_failure_in_trailers() {
        let _trace = trace_init();

        let retries = Arc::new(AtomicUsize::new(0));

        let srv = server::http2().route_async("/retry", {
            let retries = retries.clone();
            move |_| {
                let success = retries.fetch_add(1, Ordering::Relaxed) > 0;
                async move {
                    let status = if success {
                        GRPC_STATUS_OK.clone()
                    } else {
                        GRPC_STATUS_UNAVAILABLE.clone()
                    };
                    let mut trailers = HeaderMap::with_capacity(1);
                    trailers.insert(GRPC_STATUS.clone(), status);
                    tracing::debug!(?trailers);
                    let (mut tx, body) = hyper::body::Body::channel();
                    tx.send_trailers(trailers).await.unwrap();
                    Ok::<_, Error>(Response::builder().status(200).body(body).unwrap())
                }
            }
        });

        let test = TestBuilder::new(srv)
            .with_profile_route(controller::route().request_path("/retry").retryable(true))
            .no_default_routes()
            .run()
            .await;
        let client = &test.client;

        let req = client
            .request_builder("/retry")
            .header(http::header::CONTENT_TYPE, "application/grpc");
        let res = client.request(req).await.unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.headers().get(&GRPC_STATUS), None);

        let mut body = res.into_body();
        let trailers = trailers(&mut body).await;
        assert_eq!(trailers.get(&GRPC_STATUS), Some(&GRPC_STATUS_OK));
        assert_eq!(retries.load(Ordering::Relaxed), 2);
    }

    /// Tests that a gRPC request is not retried when the initial request
    /// succeeds, and that the successful response's body is not eaten.
    #[tokio::test]
    async fn does_not_retry_success_in_trailers() {
        let _trace = trace_init();

        let retries = Arc::new(AtomicUsize::new(0));

        let srv = server::http2().route_async("/retry", {
            let retries = retries.clone();
            move |_| {
                retries.fetch_add(1, Ordering::Relaxed);
                async move {
                    let mut trailers = HeaderMap::with_capacity(1);
                    trailers.insert(GRPC_STATUS.clone(), GRPC_STATUS_OK.clone());
                    tracing::debug!(?trailers);
                    let (mut tx, body) = hyper::body::Body::channel();
                    tx.send_data("hello world".into()).await.unwrap();
                    tx.send_trailers(trailers).await.unwrap();
                    Ok::<_, Error>(Response::builder().status(200).body(body).unwrap())
                }
            }
        });

        let test = TestBuilder::new(srv)
            .with_profile_route(controller::route().request_path("/retry").retryable(true))
            .no_default_routes()
            .run()
            .await;
        let client = &test.client;

        let res = client
            .request(client.request_builder("/retry"))
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.headers().get(&GRPC_STATUS), None);

        let mut body = res.into_body();

        let data = data(&mut body).await;
        assert_eq!(data, Bytes::from("hello world"));

        let trailers = trailers(&mut body).await;
        assert_eq!(trailers.get(&GRPC_STATUS), Some(&GRPC_STATUS_OK));
        assert_eq!(retries.load(Ordering::Relaxed), 1);
    }

    /// Tests that when waiting for the response body's trailers, we won't drop
    /// any DATA frames after the first one.
    #[tokio::test]
    async fn does_not_eat_multiple_data_frame_body() {
        let _trace = trace_init();

        let retries = Arc::new(AtomicUsize::new(0));

        let srv = server::http2().route_async("/retry", {
            let retries = retries.clone();
            move |_| {
                retries.fetch_add(1, Ordering::Relaxed);
                async move {
                    let mut trailers = HeaderMap::with_capacity(1);
                    trailers.insert(GRPC_STATUS.clone(), GRPC_STATUS_OK.clone());
                    tracing::debug!(?trailers);
                    let (mut tx, body) = hyper::body::Body::channel();
                    tokio::spawn(async move {
                        tx.send_data("hello".into()).await.unwrap();
                        tx.send_data("world".into()).await.unwrap();
                        tx.send_trailers(trailers).await.unwrap();
                    });
                    Ok::<_, Error>(Response::builder().status(200).body(body).unwrap())
                }
            }
        });

        let test = TestBuilder::new(srv)
            .with_profile_route(controller::route().request_path("/retry").retryable(true))
            .no_default_routes()
            .run()
            .await;
        let client = &test.client;

        let res = client
            .request(client.request_builder("/retry"))
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.headers().get(&GRPC_STATUS), None);

        let mut body = res.into_body();

        let frame1 = data(&mut body).await;
        assert_eq!(frame1, Bytes::from("hello"));

        let frame2 = data(&mut body).await;
        assert_eq!(frame2, Bytes::from("world"));

        let trailers = trailers(&mut body).await;
        assert_eq!(trailers.get(&GRPC_STATUS), Some(&GRPC_STATUS_OK));
        assert_eq!(retries.load(Ordering::Relaxed), 1);
    }

    async fn data(body: &mut hyper::Body) -> Bytes {
        let data = body
            .data()
            .await
            .expect("body data frame must not be eaten")
            .unwrap();
        tracing::info!(?data);
        data
    }
    async fn trailers(body: &mut hyper::Body) -> http::HeaderMap {
        let trailers = body
            .trailers()
            .await
            .expect("trailers future should not fail")
            .expect("response should have trailers");
        tracing::info!(?trailers);
        trailers
    }
}
