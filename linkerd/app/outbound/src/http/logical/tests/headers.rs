use super::*;
use http_body_util::BodyExt;
use linkerd_app_core::{proxy::http::StatusCode, trace};
use linkerd_proxy_client_policy::http::RouteParams as HttpParams;
use tokio::time;
use tracing::{info, Instrument};

// === HTTP ===

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn http() {
    let _trace = trace::test::trace_init();

    const TIMEOUT: time::Duration = time::Duration::from_secs(2);
    let (svc, mut handle) = mock_http(HttpParams {
        allow_l5d_request_headers: true,
        ..Default::default()
    });

    info!("Sending a request that will initially fail and then succeed");
    tokio::spawn(
        async move {
            handle.allow(3);
            serve(&mut handle, async move {
                info!("Failing the first request");
                mk_rsp(StatusCode::INTERNAL_SERVER_ERROR, "").await
            })
            .await;
            serve(&mut handle, async move {
                info!("Delaying the second request");
                time::sleep(TIMEOUT * 2).await;
                mk_rsp(StatusCode::IM_A_TEAPOT, "").await
            })
            .await;
            serve(&mut handle, async move {
                info!("Serving the third request");
                mk_rsp(StatusCode::NO_CONTENT, "").await
            })
            .await;
            handle
        }
        .in_current_span(),
    );

    info!("Verifying that we see the successful response");
    let rsp = time::timeout(
        TIMEOUT * 4,
        send_req(
            svc.clone(),
            http::Request::get("/")
                .header("l5d-retry-limit", "2")
                .header("l5d-retry-http", "5xx")
                .header("l5d-retry-timeout", "100ms")
                .body(Default::default())
                .unwrap(),
        ),
    )
    .await
    .expect("response");
    assert_eq!(rsp.expect("response").status(), StatusCode::NO_CONTENT);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn http_requires_allow() {
    let _trace = trace::test::trace_init();

    const TIMEOUT: time::Duration = time::Duration::from_secs(2);
    let (svc, mut handle) = mock_http(HttpParams {
        allow_l5d_request_headers: false,
        ..Default::default()
    });

    tokio::spawn(
        async move {
            handle.allow(2);
            serve(&mut handle, mk_rsp(StatusCode::INTERNAL_SERVER_ERROR, "")).await;
            serve(&mut handle, mk_rsp(StatusCode::NO_CONTENT, "")).await;
            handle
        }
        .in_current_span(),
    );

    info!("Sending a request that will initially fail and then succeed");
    let rsp = time::timeout(
        TIMEOUT,
        send_req(
            svc.clone(),
            http::Request::get("/")
                .header("l5d-retry-limit", "1")
                .header("l5d-retry-http", "5xx")
                .body(Default::default())
                .unwrap(),
        ),
    )
    .await
    .expect("response");

    info!("Verifying that we see the successful response");
    assert_eq!(
        rsp.expect("response").status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

// === gRPC ===

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn grpc() {
    let _trace = trace::test::trace_init();

    const TIMEOUT: time::Duration = time::Duration::from_secs(2);
    let (svc, mut handle) = mock_http(HttpParams {
        allow_l5d_request_headers: true,
        ..Default::default()
    });

    info!("Sending a request that will initially fail and then succeed");
    tokio::spawn(
        async move {
            handle.allow(3);
            serve(&mut handle, async move {
                info!("Failing the first request");
                mk_grpc_rsp(tonic::Code::Unavailable).await
            })
            .await;
            serve(&mut handle, async move {
                info!("Delaying the second request");
                time::sleep(TIMEOUT).await;
                mk_grpc_rsp(tonic::Code::NotFound).await
            })
            .await;
            serve(&mut handle, async move {
                info!("Serving the third request");
                mk_grpc_rsp(tonic::Code::Ok).await
            })
            .await;
            handle
        }
        .in_current_span(),
    );

    info!("Verifying that we see the successful response");
    let (parts, mut body) = time::timeout(
        TIMEOUT * 4,
        send_req(
            svc.clone(),
            http::Request::get("/")
                .version(::http::Version::HTTP_2)
                .header("content-type", "application/grpc")
                .header("l5d-retry-limit", "2")
                .header("l5d-retry-grpc", "unavailable")
                .header("l5d-retry-timeout", "100ms")
                .body(Default::default())
                .unwrap(),
        ),
    )
    .await
    .expect("response")
    .expect("response")
    .into_parts();
    assert_eq!(parts.status, StatusCode::OK);
    let trailers = body
        .frame()
        .await
        .expect("a result")
        .expect("a frame")
        .into_trailers()
        .ok()
        .expect("trailers frame");
    assert_eq!(
        trailers
            .get("grpc-status")
            .expect("grpc-status")
            .to_str()
            .unwrap()
            .parse::<u8>()
            .unwrap(),
        tonic::Code::Ok as u8
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn grpc_requires_allow() {
    let _trace = trace::test::trace_init();

    const TIMEOUT: time::Duration = time::Duration::from_secs(2);
    let (svc, mut handle) = mock_http(HttpParams {
        allow_l5d_request_headers: false,
        ..Default::default()
    });

    info!("Sending a request that will initially fail and then succeed");
    tokio::spawn(
        async move {
            handle.allow(3);
            serve(&mut handle, async move {
                info!("Failing the first request");
                mk_grpc_rsp(tonic::Code::Unavailable).await
            })
            .await;
            serve(&mut handle, async move {
                info!("Delaying the second request");
                time::sleep(TIMEOUT).await;
                mk_grpc_rsp(tonic::Code::NotFound).await
            })
            .await;
            serve(&mut handle, async move {
                info!("Serving the third request");
                mk_grpc_rsp(tonic::Code::Ok).await
            })
            .await;
            handle
        }
        .in_current_span(),
    );

    info!("Verifying that we see the successful response");
    let (parts, mut body) = time::timeout(
        TIMEOUT * 4,
        send_req(
            svc.clone(),
            http::Request::get("/")
                .version(::http::Version::HTTP_2)
                .header("content-type", "application/grpc")
                .header("l5d-retry-limit", "2")
                .header("l5d-retry-grpc", "unavailable")
                .header("l5d-retry-timeout", "100ms")
                .body(Default::default())
                .unwrap(),
        ),
    )
    .await
    .expect("response")
    .expect("response")
    .into_parts();
    assert_eq!(parts.status, StatusCode::OK);
    let trailers = body
        .frame()
        .await
        .expect("a result")
        .expect("a frame")
        .into_trailers()
        .ok()
        .expect("trailers frame");
    assert_eq!(
        trailers
            .get("grpc-status")
            .expect("grpc-status")
            .to_str()
            .unwrap()
            .parse::<u8>()
            .unwrap(),
        tonic::Code::Unavailable as u8
    );
}

// === List whitespace ===

/// A spaced `l5d-retry-http` list retries on every listed status.
///
/// Sets up `5xx, 429` with header overrides allowed and a backend that returns 429, then 204.
/// Catches the second list item being dropped, which returns the 429 without a retry.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn http_retry_list_with_space() {
    let _trace = trace::test::trace_init();

    // Allow l5d request headers with no route retry config, so only the header drives retries.
    const TIMEOUT: time::Duration = time::Duration::from_secs(2);
    let (svc, mut handle) = mock_http(HttpParams {
        allow_l5d_request_headers: true,
        ..Default::default()
    });

    // Serve a retryable 429 first, then the success a retry would reach.
    tokio::spawn(
        async move {
            handle.allow(2);
            serve(&mut handle, mk_rsp(StatusCode::TOO_MANY_REQUESTS, "")).await;
            serve(&mut handle, mk_rsp(StatusCode::NO_CONTENT, "")).await;
            handle
        }
        .in_current_span(),
    );

    // RFC 9110 section 5.6.1 allows OWS after the list comma, so 429 must be retryable.
    let rsp = time::timeout(
        TIMEOUT,
        send_req(
            svc.clone(),
            http::Request::get("/")
                .header("l5d-retry-http", "5xx, 429")
                .body(Default::default())
                .unwrap(),
        ),
    )
    .await
    .expect("response");
    assert_eq!(
        rsp.expect("response").status(),
        StatusCode::NO_CONTENT,
        "429 listed after a spaced comma must be retried"
    );
}

/// An `l5d-retry-http` value with no valid item keeps the route's retry statuses.
///
/// Sets up a route retrying 5xx, header `teapot`, and a backend that returns 500, then 204.
/// Catches the invalid header replacing the route's statuses with none, which returns the 500.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn http_retry_all_invalid_keeps_route_config() {
    let _trace = trace::test::trace_init();

    // Configure route retries on the default 5xx range and allow l5d request headers.
    const TIMEOUT: time::Duration = time::Duration::from_secs(2);
    let (svc, mut handle) = mock_http(HttpParams {
        allow_l5d_request_headers: true,
        retry: Some(client_policy::http::Retry {
            max_retries: 1,
            status_ranges: Default::default(),
            max_request_bytes: 1000,
            timeout: None,
            backoff: None,
        }),
        ..Default::default()
    });

    // Serve a 500 the route config retries, then the success a retry would reach.
    tokio::spawn(
        async move {
            handle.allow(2);
            serve(&mut handle, mk_rsp(StatusCode::INTERNAL_SERVER_ERROR, "")).await;
            serve(&mut handle, mk_rsp(StatusCode::NO_CONTENT, "")).await;
            handle
        }
        .in_current_span(),
    );

    // A header with nothing parseable must leave the route's statuses in effect.
    let rsp = time::timeout(
        TIMEOUT,
        send_req(
            svc.clone(),
            http::Request::get("/")
                .header("l5d-retry-http", "teapot")
                .body(Default::default())
                .unwrap(),
        ),
    )
    .await
    .expect("response");
    assert_eq!(
        rsp.expect("response").status(),
        StatusCode::NO_CONTENT,
        "an all-invalid header must not disable the route's 5xx retries"
    );
}

/// A spaced `l5d-retry-grpc` list retries on every listed code.
///
/// Sets up `unavailable, cancelled` with header overrides allowed and a backend that returns
/// CANCELLED, then OK.
/// Catches the second list item being dropped, which returns CANCELLED without a retry.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn grpc_retry_list_with_space() {
    let _trace = trace::test::trace_init();

    // Allow l5d request headers with no route retry config, so only the header drives retries.
    const TIMEOUT: time::Duration = time::Duration::from_secs(2);
    let (svc, mut handle) = mock_http(HttpParams {
        allow_l5d_request_headers: true,
        ..Default::default()
    });

    // Serve a retryable CANCELLED first, then the success a retry would reach.
    tokio::spawn(
        async move {
            handle.allow(2);
            serve(&mut handle, mk_grpc_rsp(tonic::Code::Cancelled)).await;
            serve(&mut handle, mk_grpc_rsp(tonic::Code::Ok)).await;
            handle
        }
        .in_current_span(),
    );

    // RFC 9110 section 5.6.1 allows OWS after the list comma, so CANCELLED must be retryable.
    let (parts, mut body) = time::timeout(
        TIMEOUT,
        send_req(
            svc.clone(),
            http::Request::get("/")
                .version(::http::Version::HTTP_2)
                .header("content-type", "application/grpc")
                .header("l5d-retry-grpc", "unavailable, cancelled")
                .body(Default::default())
                .unwrap(),
        ),
    )
    .await
    .expect("response")
    .expect("response")
    .into_parts();
    assert_eq!(parts.status, StatusCode::OK);

    // The gRPC outcome is in the trailers; OK proves the retry reached the second response.
    let trailers = body
        .frame()
        .await
        .expect("a result")
        .expect("a frame")
        .into_trailers()
        .ok()
        .expect("trailers frame");
    assert_eq!(
        trailers
            .get("grpc-status")
            .expect("grpc-status")
            .to_str()
            .unwrap()
            .parse::<u8>()
            .unwrap(),
        tonic::Code::Ok as u8,
        "CANCELLED listed after a spaced comma must be retried"
    );
}

/// An `l5d-retry-grpc` value with no valid item keeps the route's retry codes.
///
/// Sets up a route retrying UNAVAILABLE, header `unavailble`, and a backend that returns
/// UNAVAILABLE, then OK.
/// Catches the misspelled header replacing the route's codes with none, which returns UNAVAILABLE.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn grpc_retry_all_invalid_keeps_route_config() {
    let _trace = trace::test::trace_init();

    // Configure route retries on UNAVAILABLE and allow l5d request headers.
    const TIMEOUT: time::Duration = time::Duration::from_secs(2);
    let (svc, mut handle) = mock_grpc(client_policy::grpc::RouteParams {
        allow_l5d_request_headers: true,
        retry: Some(client_policy::grpc::Retry {
            max_retries: 1,
            codes: client_policy::grpc::Codes(
                Some(tonic::Code::Unavailable as u16)
                    .into_iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .into(),
            ),
            max_request_bytes: 1000,
            timeout: None,
            backoff: None,
        }),
        ..Default::default()
    });

    // Serve an UNAVAILABLE the route config retries, then the success a retry would reach.
    tokio::spawn(
        async move {
            handle.allow(2);
            serve(&mut handle, mk_grpc_rsp(tonic::Code::Unavailable)).await;
            serve(&mut handle, mk_grpc_rsp(tonic::Code::Ok)).await;
            handle
        }
        .in_current_span(),
    );

    // A header with nothing parseable must leave the route's codes in effect.
    let (parts, mut body) = time::timeout(
        TIMEOUT,
        send_req(
            svc.clone(),
            http::Request::post("/svc/method")
                .header("l5d-retry-grpc", "unavailble")
                .body(Default::default())
                .unwrap(),
        ),
    )
    .await
    .expect("response")
    .expect("response")
    .into_parts();
    assert_eq!(parts.status, StatusCode::OK);

    // The gRPC outcome is in the trailers; OK proves the route's retry still ran.
    let trailers = body
        .frame()
        .await
        .expect("a result")
        .expect("a frame")
        .into_trailers()
        .ok()
        .expect("trailers frame");
    assert_eq!(
        trailers
            .get("grpc-status")
            .expect("grpc-status")
            .to_str()
            .unwrap()
            .parse::<u8>()
            .unwrap(),
        tonic::Code::Ok as u8,
        "an all-invalid header must not disable the route's UNAVAILABLE retries"
    );
}
