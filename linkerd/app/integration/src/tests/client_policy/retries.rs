use super::*;
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

#[tokio::test]
async fn http1_retries() {
    test_basic_retry(client::http1, server::http1).await
}

#[tokio::test]
async fn http2_retries() {
    test_basic_retry(client::http1, server::http1).await
}

#[tokio::test]
async fn http1_doesnt_retry_non_retryable_rule() {
    test_non_retryable(client::http1, server::http1).await
}

#[tokio::test]
async fn http2_doesnt_retry_non_retryable_rule() {
    test_non_retryable(client::http2, server::http2).await
}

#[tokio::test]
async fn http1_doesnt_retry_at_per_request_limit() {
    test_retry_limit(client::http1, server::http1).await
}

#[tokio::test]
async fn http2_doesnt_retry_at_per_request_limit() {
    test_retry_limit(client::http2, server::http2).await
}

#[tokio::test]
async fn http1_retries_post_body() {
    test_retry_body(client::http1, server::http1, http::Method::POST).await
}

#[tokio::test]
async fn http2_retries_post_body() {
    test_retry_body(client::http2, server::http2, http::Method::POST).await
}

#[tokio::test]
async fn http1_retries_put_body() {
    test_retry_body(client::http1, server::http1, http::Method::PUT).await
}

#[tokio::test]
async fn http2_retries_put_body() {
    test_retry_body(client::http2, server::http2, http::Method::PUT).await
}

#[tokio::test]
async fn http1_doesnt_retry_long_body() {
    test_too_long_retry_body(client::http1, server::http1).await
}

#[tokio::test]
async fn http2_doesnt_retry_long_body() {
    test_too_long_retry_body(client::http2, server::http2).await
}

#[tokio::test]
async fn http1_backend_request_timeout_is_per_try() {
    test_backend_timeout_is_per_try(client::http1, server::http1).await
}

#[tokio::test]
async fn http2_backend_request_timeout_is_per_try() {
    test_backend_timeout_is_per_try(client::http2, server::http2).await
}

#[tokio::test]
async fn http1_request_timeout_across_retries() {
    test_request_timeout_across_retries(client::http1, server::http1).await
}

#[tokio::test]
async fn http2_request_timeout_across_retries() {
    test_request_timeout_across_retries(client::http2, server::http2).await
}

async fn test_basic_retry(
    mk_client: fn(addr: SocketAddr, auth: &'static str) -> client::Client,
    mk_server: fn() -> server::Server,
) {
    let srv = retry_server(mk_server, 1).run().await;
    run_retry_test(mk_client, srv, |client| async move {
        let rsp = client
            .request(client.request_builder("/retry"))
            .await
            .unwrap();
        assert_eq!(
            rsp.status(),
            http::StatusCode::OK,
            "retry route should succeed"
        );
    })
    .await
}

async fn test_non_retryable(
    mk_client: fn(addr: SocketAddr, auth: &'static str) -> client::Client,
    mk_server: fn() -> server::Server,
) {
    let srv = retry_server(mk_server, 1).run().await;
    run_retry_test(mk_client, srv, |client| async move {
        let rsp = client
            .request(client.request_builder("/no-retry"))
            .await
            .unwrap();
        assert_eq!(
            rsp.status(),
            http::StatusCode::INTERNAL_SERVER_ERROR,
            "non-retryable route should not be retried"
        );
    })
    .await;
}

async fn test_retry_limit(
    mk_client: fn(addr: SocketAddr, auth: &'static str) -> client::Client,
    mk_server: fn() -> server::Server,
) {
    // fail 4 times, so that the first request will reach the per-request limit
    // of 3 retries.
    let srv = retry_server(mk_server, 4).run().await;
    run_retry_test(mk_client, srv, |client| async move {
        let rsp = client
            .request(client.request_builder("/retry"))
            .await
            .unwrap();
        assert_eq!(
            rsp.status(),
            http::StatusCode::INTERNAL_SERVER_ERROR,
            "retries should stop when at the per-request limit"
        );

        let rsp = client
            .request(client.request_builder("/retry"))
            .await
            .unwrap();
        assert_eq!(
            rsp.status(),
            http::StatusCode::OK,
            "retry limit should be tracked per-request"
        );
    })
    .await
}

async fn test_retry_body(
    mk_client: fn(addr: SocketAddr, auth: &'static str) -> client::Client,
    mk_server: fn() -> server::Server,
    method: http::Method,
) {
    let srv = retry_server(mk_server, 1).run().await;
    run_retry_test(mk_client, srv, move |client| async move {
        let req = client
            .request_builder("/retry")
            .method(method.clone())
            .body("i'm a request body".into())
            .unwrap();
        let rsp = client.request_body(req).await;

        assert_eq!(
            rsp.status(),
            http::StatusCode::OK,
            "{method:?} request with body should be retried"
        );
    })
    .await
}

async fn test_too_long_retry_body(
    mk_client: fn(addr: SocketAddr, auth: &'static str) -> client::Client,
    mk_server: fn() -> server::Server,
) {
    let srv = retry_server(mk_server, 1).run().await;
    run_retry_test(mk_client, srv, move |client| async move {
        let req = client
            .request_builder("/retry")
            .method(http::Method::POST)
            .body(hyper::Body::from(&[1u8; 64 * 1024 + 1][..]))
            .unwrap();
        let rsp = client.request_body(req).await;

        assert_eq!(
            rsp.status(),
            http::StatusCode::INTERNAL_SERVER_ERROR,
            "request with too long body should not be retried"
        );
    })
    .await
}

async fn test_backend_timeout_is_per_try(
    mk_client: fn(addr: SocketAddr, auth: &'static str) -> client::Client,
    mk_server: fn() -> server::Server,
) {
    let srv = retry_server(mk_server, 1).run().await;
    run_retry_timeout_test(
        mk_client,
        srv,
        None,
        Some(Duration::from_millis(400)),
        move |client| async move {
            let req = client
                .request_builder("/retry/timeout")
                .method(http::Method::GET)
                .body("".into())
                .unwrap();
            let rsp = client.request_body(req).await;
            tracing::info!(?rsp);
            assert_eq!(
                rsp.status(),
                http::StatusCode::OK,
                "backend request timeouts should be retried"
            );
        },
    )
    .await
}

async fn test_request_timeout_across_retries(
    mk_client: fn(addr: SocketAddr, auth: &'static str) -> client::Client,
    mk_server: fn() -> server::Server,
) {
    let srv = retry_server(mk_server, 2).run().await;
    run_retry_timeout_test(
        mk_client,
        srv,
        Some(Duration::from_millis(800)),
        Some(Duration::from_millis(400)),
        move |client| async move {
            let req = client
                .request_builder("/retry/timeout")
                .method(http::Method::GET)
                .body("".into())
                .unwrap();
            let rsp = client.request_body(req).await;

            assert_eq!(
                rsp.status(),
                http::StatusCode::GATEWAY_TIMEOUT,
                "the request timeout should be tracked across retries"
            );
        },
    )
    .await
}

async fn run_retry_test<F: Future<Output = ()>>(
    mk_client: fn(addr: SocketAddr, auth: &'static str) -> client::Client,
    srv: server::Listening,
    test: impl FnOnce(client::Client) -> F,
) {
    run_retry_timeout_test(mk_client, srv, None, None, test).await
}

async fn run_retry_timeout_test<F: Future<Output = ()>>(
    mk_client: fn(addr: SocketAddr, auth: &'static str) -> client::Client,
    srv: server::Listening,
    request_timeout: Option<Duration>,
    backend_timeout: Option<Duration>,
    test: impl FnOnce(client::Client) -> F,
) {
    let _trace = trace_init();

    const AUTHORITY: &str = "policy.test.svc.cluster.local";
    let ctrl = controller::new();

    let dst = format!("{AUTHORITY}:{}", srv.addr.port());
    let dst_tx = ctrl.destination_tx(&dst);
    dst_tx.send_addr(srv.addr);
    let _profile_tx = ctrl.profile_tx_default(srv.addr, AUTHORITY);
    let policy = controller::policy()
        // stop the admin server from entering an infinite retry loop
        .with_inbound_default(policy::all_unauthenticated())
        .outbound(
            srv.addr,
            retry_policy(dst, request_timeout, backend_timeout),
        );

    let proxy = proxy::new()
        .controller(ctrl.run().await)
        .policy(policy.run().await)
        .outbound(srv)
        .run()
        .await;
    let client = mk_client(proxy.outbound, AUTHORITY);
    test(client).await;

    // ensure panics from the server are propagated
    proxy.join_servers().await;
}

fn retry_server(srv: fn() -> server::Server, fails: usize) -> server::Server {
    srv()
        .route_fn("/retry", {
            let retries = AtomicUsize::new(0);
            move |_| {
                let attempt = retries.fetch_add(1, Ordering::SeqCst);
                tracing::info!(attempt, "/retry");
                let status = if attempt > fails {
                    http::StatusCode::OK
                } else {
                    http::StatusCode::INTERNAL_SERVER_ERROR
                };
                http::Response::builder()
                    .status(status)
                    .body("".into())
                    .unwrap()
            }
        })
        .route_async("/retry/timeout", {
            let retries = Arc::new(AtomicUsize::new(0));
            move |_| {
                let retries = retries.clone();
                async move {
                    let attempt = retries.fetch_add(1, Ordering::SeqCst);
                    tracing::info!(attempt, "/retry/timeout");
                    if attempt < fails {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    };
                    http::Response::builder()
                        .status(http::StatusCode::OK)
                        .body("".into())
                }
            }
        })
        .route_fn("/no-retry", move |_| {
            tracing::info!("/no-retry");
            http::Response::builder()
                .status(http::StatusCode::INTERNAL_SERVER_ERROR)
                .body("".into())
                .unwrap()
        })
}

fn retry_policy(
    dst: impl ToString,
    request_timeout: Option<Duration>,
    backend_timeout: Option<Duration>,
) -> outbound::OutboundPolicy {
    use outbound::http_route::{self, distribution};
    let dst = dst.to_string();

    let dist = http_route::Distribution {
        kind: Some(distribution::Kind::FirstAvailable(
            distribution::FirstAvailable {
                backends: vec![http_route::RouteBackend {
                    backend: Some(policy::backend(dst.clone())),
                    filters: Vec::new(),
                    request_timeout: backend_timeout.map(|t| t.try_into().unwrap()),
                }],
            },
        )),
    };

    let routes = vec![outbound::HttpRoute {
        metadata: Some(httproute_meta("retry")),
        hosts: Vec::new(),
        rules: vec![
            // this rule is retryable
            outbound::http_route::Rule {
                matches: vec![policy::match_path_prefix("/retry")],
                filters: Vec::new(),
                backends: Some(dist.clone()),
                request_timeout: request_timeout.map(|t| t.try_into().unwrap()),
                retry_policy: Some(outbound::http_route::RetryPolicy {
                    retry_statuses: vec![controller::pb::HttpStatusRange { min: 500, max: 599 }],
                    max_per_request: 3,
                }),
            },
            // this route is not retryable
            outbound::http_route::Rule {
                matches: vec![policy::match_path_prefix("/no-retry")],
                filters: Vec::new(),
                backends: Some(dist),
                request_timeout: None,
                retry_policy: None,
            },
        ],
    }];

    let retry_budget = Some(controller::retry_budget(Duration::from_secs(10), 0.5, 10));

    outbound::OutboundPolicy {
        metadata: Some(api::meta::Metadata {
            kind: Some(api::meta::metadata::Kind::Default("retry-test".to_string())),
        }),
        protocol: Some(outbound::ProxyProtocol {
            kind: Some(proxy_protocol::Kind::Detect(proxy_protocol::Detect {
                timeout: Some(Duration::from_secs(10).try_into().unwrap()),
                http1: Some(proxy_protocol::Http1 {
                    routes: routes.clone(),
                    failure_accrual: None,
                    retry_budget: retry_budget.clone(),
                }),
                http2: Some(proxy_protocol::Http2 {
                    routes,
                    failure_accrual: None,
                    retry_budget,
                }),
                opaque: Some(proxy_protocol::Opaque {
                    routes: vec![policy::outbound_default_opaque_route(&dst)],
                }),
            })),
        }),
    }
}
