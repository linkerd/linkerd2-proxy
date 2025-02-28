use super::*;
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
    .map(linkerd_http_body_compat::ForwardCompatibleBody::new)
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
    .map(linkerd_http_body_compat::ForwardCompatibleBody::new)
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
