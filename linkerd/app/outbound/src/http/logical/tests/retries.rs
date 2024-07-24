use super::{
    super::{policy, Outbound, ParentRef, Routes},
    default_backend, http_get, mk_grpc_rsp, mk_rsp, send_req, serve_delayed, serve_req,
    HttpConnect, Request, Response, Target,
};
use crate::test_util::*;
use hyper::body::HttpBody;
use linkerd_app_core::{
    errors,
    proxy::http::{self, StatusCode},
    svc::{self, http::stream_timeouts::StreamDeadlineError, NewService},
    trace, NameAddr,
};
use linkerd_proxy_client_policy::{
    self as client_policy,
    grpc::{Codes, RouteParams as GrpcParams},
    http::{RouteParams as HttpParams, Timeouts},
};
use std::{collections::BTreeSet, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{sync::watch, time};
use tonic::Code;
use tracing::{info, Instrument};

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn http_5xx() {
    let _trace = trace::test::trace_init();

    const TIMEOUT: Duration = Duration::from_secs(2);
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
    });

    info!("Sending a request that will initially fail and then succeed");
    tokio::spawn(
        async move {
            handle.allow(2);
            info!("Failing the first request");
            serve_req(&mut handle, mk_rsp(StatusCode::INTERNAL_SERVER_ERROR, "")).await;
            info!("Serving the second request");
            serve_req(&mut handle, mk_rsp(StatusCode::NO_CONTENT, "")).await;
            handle
        }
        .in_current_span(),
    );

    info!("Verifying that we see the successful response");
    let rsp = time::timeout(TIMEOUT, send_req(svc.clone(), http_get()))
        .await
        .expect("response");
    assert_eq!(rsp.expect("response").status(), StatusCode::NO_CONTENT);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn http_5xx_limits() {
    let _trace = trace::test::trace_init();

    const TIMEOUT: Duration = Duration::from_secs(2);
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
    });

    info!("Sending a request that will initially fail and then succeed");
    tokio::spawn(
        async move {
            handle.allow(3);
            info!("Failing the first request");
            serve_req(&mut handle, mk_rsp(StatusCode::INTERNAL_SERVER_ERROR, "")).await;
            info!("Failing the second request");
            serve_req(&mut handle, mk_rsp(StatusCode::INTERNAL_SERVER_ERROR, "")).await;
            info!("Failing the third request");
            serve_req(&mut handle, mk_rsp(StatusCode::GATEWAY_TIMEOUT, "")).await;
            info!("Prepping the fourth request (shouldn't be served)");
            serve_req(&mut handle, mk_rsp(StatusCode::NO_CONTENT, "")).await;
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

    const TIMEOUT: Duration = Duration::from_secs(2);
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
    });

    info!("Sending a request that will initially timeout and then succeed");
    tokio::spawn(
        async move {
            handle.allow(2);

            info!("Delaying the first request");
            serve_delayed(
                TIMEOUT / 2,
                &mut handle,
                Ok(mk_rsp(StatusCode::NOT_FOUND, "")),
            )
            .await;

            info!("Serving the second request");
            serve_req(&mut handle, mk_rsp(StatusCode::NO_CONTENT, "")).await;

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

    const TIMEOUT: Duration = Duration::from_secs(2);
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
    });

    info!("Testing that a retry timeout does not apply when max retries is reached");
    tokio::spawn(
        async move {
            handle.allow(2);

            info!("Delaying the first request");
            serve_delayed(
                TIMEOUT / 3,
                &mut handle,
                Ok(mk_rsp(StatusCode::NOT_FOUND, "")),
            )
            .await;

            info!("Delaying the second request");
            serve_delayed(
                TIMEOUT / 3,
                &mut handle,
                Ok(mk_rsp(StatusCode::NO_CONTENT, "")),
            )
            .await;

            handle
        }
        .in_current_span(),
    );

    info!("Verifying that the initial request was retried");
    let rsp = time::timeout(TIMEOUT, send_req(svc.clone(), http_get()))
        .await
        .expect("response");
    assert_eq!(rsp.expect("response").status(), StatusCode::NO_CONTENT);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn http_timeout_with_request_timeout() {
    let _trace = trace::test::trace_init();

    const TIMEOUT: Duration = Duration::from_millis(100);
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
    });

    info!("Sending a request that will initially timeout and then succeed");
    tokio::spawn(
        async move {
            handle.allow(6);

            info!("Delaying the first request");
            serve_delayed(
                TIMEOUT * 2,
                &mut handle,
                Ok(mk_rsp(StatusCode::IM_A_TEAPOT, "")),
            )
            .await;

            info!("Delaying the second request");
            serve_delayed(
                TIMEOUT * 2,
                &mut handle,
                Ok(mk_rsp(StatusCode::IM_A_TEAPOT, "")),
            )
            .await;

            info!("Delaying the third request");
            serve_req(&mut handle, mk_rsp(StatusCode::NO_CONTENT, "")).await;

            info!("Delaying the fourth request");
            serve_delayed(
                TIMEOUT * 2,
                &mut handle,
                Ok(mk_rsp(StatusCode::IM_A_TEAPOT, "")),
            )
            .await;

            info!("Delaying the fifth request");
            serve_delayed(
                TIMEOUT * 2,
                &mut handle,
                Ok(mk_rsp(StatusCode::IM_A_TEAPOT, "")),
            )
            .await;

            info!("Delaying the sixth request");
            serve_delayed(
                TIMEOUT * 5,
                &mut handle,
                Ok(mk_rsp(StatusCode::NO_CONTENT, "")),
            )
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

    const TIMEOUT: Duration = Duration::from_millis(100);
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
    });

    info!("Sending a request that will initially fail and then succeed");
    tokio::spawn(
        async move {
            handle.allow(2);
            info!("Failing the first request");
            serve_req(&mut handle, mk_grpc_rsp(tonic::Code::Internal)).await;
            info!("Serving the second request");
            serve_req(&mut handle, mk_grpc_rsp(tonic::Code::Ok)).await;
            handle
        }
        .in_current_span(),
    );

    info!("Verifying that we see the successful response");
    let mut rsp = time::timeout(
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
    .expect("response ok");
    assert_eq!(rsp.status(), StatusCode::OK);
    while rsp.body_mut().data().await.is_some() {}
    let trailers = rsp.body_mut().trailers().await;
    assert_eq!(
        trailers
            .expect("trailers")
            .expect("trailers")
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

    const TIMEOUT: Duration = Duration::from_millis(100);
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
    });

    info!("Sending a request that will initially fail and then succeed");
    tokio::spawn(
        async move {
            handle.allow(2);
            info!("Delaying the first request");
            serve_delayed(
                TIMEOUT * 2,
                &mut handle,
                Ok(mk_grpc_rsp(tonic::Code::NotFound)),
            )
            .await;
            info!("Serving the second request");
            serve_req(&mut handle, mk_grpc_rsp(tonic::Code::Ok)).await;
            handle
        }
        .in_current_span(),
    );

    info!("Verifying that we see the successful response");
    let mut rsp = time::timeout(
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
    .expect("response ok");
    assert_eq!(rsp.status(), StatusCode::OK);
    while rsp.body_mut().data().await.is_some() {}
    let trailers = rsp.body_mut().trailers().await;
    assert_eq!(
        trailers
            .expect("trailers")
            .expect("trailers")
            .get("grpc-status")
            .expect("grpc-status")
            .to_str()
            .unwrap(),
        "0"
    );
}

// === Utils ===

type Handle = tower_test::mock::Handle<Request, Response>;

fn mock_http(params: HttpParams) -> (svc::BoxCloneHttp, Handle) {
    let dest: NameAddr = "example.com:1234".parse::<NameAddr>().unwrap();
    let backend = default_backend(&dest);
    let route = mk_route(backend.clone(), params);
    mock(
        dest.clone(),
        policy::Params::Http(policy::HttpParams {
            addr: dest.into(),
            meta: ParentRef(client_policy::Meta::new_default("parent")),
            backends: Arc::new([backend]),
            routes: Arc::new([route]),
            failure_accrual: client_policy::FailureAccrual::None,
        }),
    )
}

fn mock_grpc(params: GrpcParams) -> (svc::BoxCloneHttp, Handle) {
    let dest: NameAddr = "example.com:1234".parse::<NameAddr>().unwrap();
    let backend = default_backend(&dest);
    let route = mk_route(backend.clone(), params);
    mock(
        dest.clone(),
        policy::Params::Grpc(policy::GrpcParams {
            addr: dest.into(),
            meta: ParentRef(client_policy::Meta::new_default("parent")),
            backends: Arc::new([backend]),
            routes: Arc::new([route]),
            failure_accrual: client_policy::FailureAccrual::None,
        }),
    )
}

fn mock(dest: NameAddr, params: policy::Params) -> (svc::BoxCloneHttp, Handle) {
    let addr = SocketAddr::new([192, 0, 2, 41].into(), 1234);

    let (inner, handle) = tower_test::mock::pair();
    let connect = HttpConnect::default().service(addr, inner);
    let resolve = support::resolver().endpoint_exists(dest.clone(), addr, Default::default());
    let (rt, _shutdown) = runtime();
    let stack = Outbound::new(default_config(), rt, &mut Default::default())
        .with_stack(svc::ArcNewService::new(connect))
        .push_http_cached(resolve)
        .into_inner();

    let (tx, routes) = watch::channel(Routes::Policy(params));
    tokio::spawn(async move {
        tx.closed().await;
    });

    let svc = stack.new_service(Target {
        num: 1,
        version: http::Version::H2,
        routes,
    });

    (svc, handle)
}

fn mk_route<M: Default, F, P>(
    backend: client_policy::Backend,
    params: P,
) -> client_policy::route::Route<M, client_policy::RoutePolicy<F, P>> {
    use client_policy::*;

    route::Route {
        hosts: vec![],
        rules: vec![route::Rule {
            matches: vec![M::default()],
            policy: RoutePolicy {
                meta: Meta::new_default("route"),
                filters: [].into(),
                distribution: RouteDistribution::FirstAvailable(Arc::new([RouteBackend {
                    filters: [].into(),
                    backend,
                }])),
                params,
            },
        }],
    }
}
