use super::*;
use linkerd_app_core::{
    errors,
    proxy::http::{self, StatusCode},
    svc::http::stream_timeouts::StreamDeadlineError,
    trace,
};
use linkerd_proxy_client_policy::{
    self as client_policy,
    grpc::{Codes, RouteParams as GrpcParams},
    http::{RouteParams as HttpParams, Timeouts},
};
use std::collections::BTreeSet;
use tokio::time;
use tonic::Code;
use tracing::{info, Instrument};

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn http_5xx() {
    let _trace = trace::test::trace_init();

    const TIMEOUT: time::Duration = time::Duration::from_secs(2);
    let (svc, mut handle) = mock_http(HttpParams {
        timeouts: Timeouts {
            request: Some(TIMEOUT),
            ..Default::default()
        },
        retry: Some(client_policy::http::Retry {
            max_retries: 1,
            status_ranges: Default::default(),
            max_request_bytes: 1000,
            timeout: None,
            backoff: None,
        }),
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
    let rsp = time::timeout(TIMEOUT, send_req(svc.clone(), http_get()))
        .await
        .expect("response");
    info!("Verifying that we see the successful response");
    assert_eq!(rsp.expect("response").status(), StatusCode::NO_CONTENT);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn http_5xx_limited() {
    let _trace = trace::test::trace_init();

    const TIMEOUT: time::Duration = time::Duration::from_secs(2);
    let (svc, mut handle) = mock_http(HttpParams {
        timeouts: Timeouts {
            request: Some(TIMEOUT),
            ..Default::default()
        },
        retry: Some(client_policy::http::Retry {
            max_retries: 2,
            status_ranges: Default::default(),
            max_request_bytes: 1000,
            timeout: None,
            backoff: None,
        }),
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
                info!("Failing the second request");
                mk_rsp(StatusCode::INTERNAL_SERVER_ERROR, "").await
            })
            .await;
            serve(&mut handle, async move {
                info!("Failing the third request");
                mk_rsp(StatusCode::GATEWAY_TIMEOUT, "").await
            })
            .await;
            info!("Prepping the fourth request (shouldn't be served)");
            serve(&mut handle, async move {
                mk_rsp(StatusCode::NO_CONTENT, "").await
            })
            .await;
            handle
        }
        .in_current_span(),
    );

    info!("Verifying that the response fails with the expected error");
    let rsp = time::timeout(TIMEOUT, send_req(svc.clone(), http_get()))
        .await
        .expect("response");
    assert_eq!(rsp.expect("response").status(), StatusCode::GATEWAY_TIMEOUT);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn http_timeout() {
    let _trace = trace::test::trace_init();

    const TIMEOUT: time::Duration = time::Duration::from_secs(2);
    let (svc, mut handle) = mock_http(HttpParams {
        timeouts: Timeouts {
            request: Some(TIMEOUT),
            ..Default::default()
        },
        retry: Some(client_policy::http::Retry {
            max_retries: 1,
            status_ranges: Default::default(),
            max_request_bytes: 1000,
            timeout: Some(TIMEOUT / 4),
            backoff: None,
        }),
        ..Default::default()
    });

    info!("Sending a request that will initially timeout and then succeed");
    tokio::spawn(
        async move {
            handle.allow(2);

            serve(&mut handle, async move {
                info!("Delaying the first request");
                time::sleep(TIMEOUT / 2).await;
                mk_rsp(StatusCode::NOT_FOUND, "").await
            })
            .await;

            serve(&mut handle, async move {
                info!("Serving the second request");
                mk_rsp(StatusCode::NO_CONTENT, "").await
            })
            .await;

            handle
        }
        .in_current_span(),
    );

    info!("Verifying that the response fails with the expected error");
    let rsp = time::timeout(TIMEOUT, send_req(svc.clone(), http_get()))
        .await
        .expect("response");
    assert_eq!(rsp.expect("response").status(), StatusCode::NO_CONTENT);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn http_timeout_on_limit() {
    let _trace = trace::test::trace_init();

    const TIMEOUT: time::Duration = time::Duration::from_secs(2);
    let (svc, mut handle) = mock_http(HttpParams {
        timeouts: Timeouts {
            request: Some(TIMEOUT),
            ..Default::default()
        },
        retry: Some(client_policy::http::Retry {
            max_retries: 1,
            status_ranges: Default::default(),
            max_request_bytes: 1000,
            timeout: Some(TIMEOUT / 4),
            backoff: None,
        }),
        ..Default::default()
    });

    tokio::spawn(
        async move {
            handle.allow(2);

            serve(&mut handle, async move {
                info!("Delaying the first request");
                time::sleep(TIMEOUT / 3).await;
                mk_rsp(StatusCode::NOT_FOUND, "").await
            })
            .await;

            serve(&mut handle, async move {
                info!("Delaying the second request");
                time::sleep(TIMEOUT / 3).await;
                mk_rsp(StatusCode::NO_CONTENT, "").await
            })
            .await;

            handle
        }
        .in_current_span(),
    );

    info!("Testing that a retry timeout does not apply when max retries is reached");
    let rsp = time::timeout(TIMEOUT, send_req(svc.clone(), http_get()))
        .await
        .expect("response");

    info!("Verifying that the initial request was retried");
    assert_eq!(rsp.expect("response").status(), StatusCode::NO_CONTENT);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn http_timeout_with_request_timeout() {
    let _trace = trace::test::trace_init();

    const TIMEOUT: time::Duration = time::Duration::from_millis(100);
    let (svc, mut handle) = mock_http(HttpParams {
        timeouts: Timeouts {
            request: Some(TIMEOUT * 5),
            ..Default::default()
        },
        retry: Some(client_policy::http::Retry {
            max_retries: 2,
            status_ranges: Default::default(),
            max_request_bytes: 1000,
            timeout: Some(TIMEOUT),
            backoff: None,
        }),
        ..Default::default()
    });

    info!("Sending a request that will initially timeout and then succeed");
    tokio::spawn(
        async move {
            handle.allow(6);

            // First request.

            serve(&mut handle, async move {
                info!("Delaying the first request");
                time::sleep(TIMEOUT * 2).await;
                mk_rsp(StatusCode::IM_A_TEAPOT, "").await
            })
            .await;

            serve(&mut handle, async move {
                info!("Delaying the second request");
                time::sleep(TIMEOUT * 2).await;
                mk_rsp(StatusCode::IM_A_TEAPOT, "").await
            })
            .await;

            serve(&mut handle, async move {
                info!("Delaying the third request");
                mk_rsp(StatusCode::NO_CONTENT, "").await
            })
            .await;

            // Second request

            serve(&mut handle, async move {
                info!("Delaying the fourth request");
                time::sleep(TIMEOUT * 2).await;
                mk_rsp(StatusCode::IM_A_TEAPOT, "").await
            })
            .await;

            serve(&mut handle, async move {
                info!("Delaying the fifth request");
                time::sleep(TIMEOUT * 2).await;
                mk_rsp(StatusCode::IM_A_TEAPOT, "").await
            })
            .await;

            serve(&mut handle, async move {
                info!("Delaying the sixth request");
                time::sleep(TIMEOUT * 5).await;
                mk_rsp(StatusCode::NO_CONTENT, "").await
            })
            .await;

            handle
        }
        .in_current_span(),
    );

    info!("Verifying that the response succeeds despite retry timeouts");
    let rsp = time::timeout(TIMEOUT * 10, send_req(svc.clone(), http_get()))
        .await
        .expect("response timed out")
        .expect("response ok");
    assert_eq!(rsp.status(), StatusCode::NO_CONTENT);

    info!("Verifying that retried requests fail with a request timeout");
    let error = time::timeout(TIMEOUT * 10, send_req(svc.clone(), http_get()))
        .await
        .expect("response timed out")
        .expect_err("response should timeout");
    assert!(errors::is_caused_by::<StreamDeadlineError>(&*error));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn grpc_internal() {
    let _trace = trace::test::with_default_filter("linkerd=debug");

    const TIMEOUT: time::Duration = time::Duration::from_millis(100);
    let (svc, mut handle) = mock_grpc(GrpcParams {
        timeouts: Timeouts {
            request: Some(TIMEOUT),
            ..Default::default()
        },
        retry: Some(client_policy::grpc::Retry {
            max_retries: 1,
            codes: Codes(
                Some(Code::Internal as u16)
                    .into_iter()
                    .collect::<BTreeSet<_>>()
                    .into(),
            ),
            max_request_bytes: 1000,
            timeout: None,
            backoff: None,
        }),
        ..Default::default()
    });

    info!("Sending a request that will initially fail and then succeed");
    tokio::spawn(
        async move {
            handle.allow(2);
            info!("Failing the first request");
            serve(&mut handle, mk_grpc_rsp(tonic::Code::Internal)).await;
            info!("Serving the second request");
            serve(&mut handle, mk_grpc_rsp(tonic::Code::Ok)).await;
            handle
        }
        .in_current_span(),
    );

    info!("Verifying that we see the successful response");
    let (parts, mut body) = time::timeout(
        TIMEOUT * 10,
        send_req(
            svc.clone(),
            http::Request::post("/svc/method")
                .body(Default::default())
                .unwrap(),
        ),
    )
    .await
    .expect("response")
    .expect("response ok")
    .map(linkerd_http_body_compat::ForwardCompatibleBody::new)
    .into_parts();
    assert_eq!(parts.status, StatusCode::OK);
    let trailers = loop {
        match body.frame().await {
            Some(Ok(frame)) => {
                if let Ok(trailers) = frame.into_trailers() {
                    break trailers;
                } else {
                    continue;
                }
            }
            None | Some(Err(_)) => panic!("body did not yield trailers"),
        }
    };
    assert_eq!(
        trailers
            .get("grpc-status")
            .expect("grpc-status")
            .to_str()
            .unwrap(),
        "0"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn grpc_timeout() {
    let _trace = trace::test::with_default_filter("linkerd=debug");

    const TIMEOUT: time::Duration = time::Duration::from_millis(100);
    let (svc, mut handle) = mock_grpc(GrpcParams {
        timeouts: Timeouts {
            request: Some(TIMEOUT * 5),
            ..Default::default()
        },
        retry: Some(client_policy::grpc::Retry {
            max_retries: 1,
            timeout: Some(TIMEOUT),
            codes: Codes(Default::default()),
            max_request_bytes: 1000,
            backoff: None,
        }),
        ..Default::default()
    });

    info!("Sending a request that will initially fail and then succeed");
    tokio::spawn(
        async move {
            handle.allow(2);
            info!("Delaying the first request");
            serve(&mut handle, async move {
                time::sleep(TIMEOUT * 2).await;
                mk_grpc_rsp(tonic::Code::NotFound).await
            })
            .await;
            info!("Serving the second request");
            serve(&mut handle, mk_grpc_rsp(tonic::Code::Ok)).await;
            handle
        }
        .in_current_span(),
    );

    info!("Verifying that we see the successful response");
    let (parts, mut body) = time::timeout(
        TIMEOUT * 10,
        send_req(
            svc.clone(),
            http::Request::post("/svc/method")
                .body(Default::default())
                .unwrap(),
        ),
    )
    .await
    .expect("response")
    .expect("response ok")
    .map(linkerd_http_body_compat::ForwardCompatibleBody::new)
    .into_parts();
    assert_eq!(parts.status, StatusCode::OK);
    let trailers = loop {
        match body.frame().await {
            Some(Ok(frame)) => {
                if let Ok(trailers) = frame.into_trailers() {
                    break trailers;
                } else {
                    continue;
                }
            }
            None | Some(Err(_)) => panic!("body did not yield trailers"),
        }
    };
    assert_eq!(
        trailers
            .get("grpc-status")
            .expect("grpc-status")
            .to_str()
            .unwrap(),
        "0"
    );
}
