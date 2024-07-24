use super::{
    super::{policy, LogicalError, Outbound, ParentRef, Routes},
    default_backend, http_get, mk_rsp, send_req, serve_delayed, serve_req, HttpConnect, Request,
    Response, Target,
};
use crate::test_util::*;
use linkerd_app_core::{
    errors,
    proxy::http::{
        self,
        stream_timeouts::{BodyTimeoutError, ResponseTimeoutError},
        BoxBody, HttpBody, StatusCode,
    },
    svc::{self, NewService},
    trace, NameAddr,
};
use linkerd_proxy_client_policy::{self as client_policy, http::Timeouts};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{sync::watch, time};
use tracing::info;

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn request_timeout_response_headers() {
    let _trace = trace::test::trace_init();

    const TIMEOUT: Duration = Duration::from_secs(2);
    let (svc, mut handle) = mock(Timeouts {
        request: Some(TIMEOUT),
        ..Default::default()
    });

    info!("Sending a request that does not respond within the timeout");
    handle.allow(1);
    let call = send_req(svc.clone(), http_get());
    serve_delayed(
        TIMEOUT * 2,
        &mut handle,
        Ok(mk_rsp(StatusCode::NO_CONTENT, "")),
    )
    .await;

    info!("Verifying that the response fails with the expected error");
    let error = time::timeout(TIMEOUT * 4, call)
        .await
        .expect("request must fail with a timeout")
        .expect_err("request must fail with a timeout");
    assert!(
        error.is::<LogicalError>(),
        "error must originate in the logical stack"
    );
    assert!(
        matches!(
            errors::cause_ref(error.as_ref()),
            Some(ResponseTimeoutError::Lifetime(_)),
        ),
        "expected response timeout, got {error}"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn request_timeout_request_body() {
    let _trace = trace::test::trace_init();

    const TIMEOUT: Duration = Duration::from_secs(2);
    let (svc, mut handle) = mock(Timeouts {
        request: Some(TIMEOUT),
        ..Default::default()
    });

    info!("Sending a request that does not respond within the timeout");
    handle.allow(1);
    let call = send_req(
        svc.clone(),
        http::Request::builder()
            .method("POST")
            .body(BoxBody::new(MockBody::new(async move {
                futures::future::pending().await
            })))
            .unwrap(),
    );

    info!("Verifying that the response fails with the expected error");
    let error = time::timeout(TIMEOUT * 2, call)
        .await
        .expect("request must fail with a timeout")
        .expect_err("request must fail with a timeout");
    assert!(
        error.is::<LogicalError>(),
        "error must originate in the logical stack"
    );
    assert!(
        matches!(
            errors::cause_ref(error.as_ref()),
            Some(ResponseTimeoutError::Lifetime(_)),
        ),
        "expected response timeout, got {error}"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn request_timeout_response_body() {
    let _trace = trace::test::trace_init();

    const TIMEOUT: Duration = Duration::from_secs(2);
    let (svc, mut handle) = mock(Timeouts {
        request: Some(TIMEOUT),
        ..Default::default()
    });

    info!("Sending a request that responds immediately but does not complete");
    handle.allow(1);
    let call = send_req(svc.clone(), http_get());
    serve_req(
        &mut handle,
        http::Response::builder()
            .status(200)
            .body(http::BoxBody::new(MockBody::new(async move {
                futures::future::pending().await
            })))
            .unwrap(),
    )
    .await;

    info!("Verifying that the request body times out with the expected stream error");
    let mut rsp = call.await.unwrap().into_body();
    let error = time::timeout(TIMEOUT * 2, rsp.data())
        .await
        .expect("should timeout internally")
        .expect("should timeout internally")
        .err()
        .expect("should timeout internally");
    assert!(
        matches!(
            errors::cause_ref(error.as_ref()),
            Some(BodyTimeoutError::Lifetime(_)),
        ),
        "expected response timeout, got {error:?}"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn response_timeout_response_headers() {
    let _trace = trace::test::with_default_filter("linkerd=trace");

    const TIMEOUT: Duration = Duration::from_secs(2);
    let (svc, mut handle) = mock(Timeouts {
        response: Some(TIMEOUT),
        ..Default::default()
    });

    info!("Sending a request that does not respond within the timeout");
    handle.allow(1);
    let call = send_req(svc.clone(), http_get());
    serve_delayed(
        TIMEOUT * 2,
        &mut handle,
        Ok(http::Response::builder()
            .status(204)
            .body(http::BoxBody::default())
            .unwrap()),
    )
    .await;

    info!("Verifying that the response fails with the expected error");
    let error = time::timeout(TIMEOUT * 4, call)
        .await
        .expect("should timeout internally")
        .expect_err("should timeout internally");
    assert!(
        matches!(
            errors::cause_ref(error.as_ref()),
            Some(ResponseTimeoutError::Response(_)),
        ),
        "expected response timeout, got {error:?}"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn response_timeout_response_body() {
    let _trace = trace::test::trace_init();

    const TIMEOUT: Duration = Duration::from_secs(2);
    let (svc, mut handle) = mock(Timeouts {
        response: Some(TIMEOUT),
        ..Default::default()
    });

    info!("Sending a request that responds immediately but does not complete");
    handle.allow(1);
    let call = send_req(svc.clone(), http_get());
    serve_req(
        &mut handle,
        http::Response::builder()
            .status(200)
            .body(http::BoxBody::new(MockBody::new(async move {
                futures::future::pending().await
            })))
            .unwrap(),
    )
    .await;

    info!("Verifying that the request body times out with the expected stream error");
    let mut rsp = call.await.unwrap().into_body();
    let error = time::timeout(TIMEOUT * 2, rsp.data())
        .await
        .expect("should timeout internally")
        .expect("should timeout internally")
        .err()
        .expect("should timeout internally");
    assert!(
        matches!(
            errors::cause_ref(error.as_ref()),
            Some(BodyTimeoutError::Response(_)),
        ),
        "expected response timeout, got {error:?}"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn response_timeout_ignores_request_body() {
    let _trace = trace::test::with_default_filter("linkerd=trace");

    const TIMEOUT: Duration = Duration::from_secs(2);
    let (svc, mut handle) = mock(Timeouts {
        response: Some(TIMEOUT),
        ..Default::default()
    });

    info!("Sending a request that exceeds the response timeout");
    handle.allow(1);
    let call = send_req(svc.clone(), http_get());
    serve_req(
        &mut handle,
        http::Response::builder()
            .status(200)
            .body(http::BoxBody::new(MockBody::new(async move {
                time::sleep(TIMEOUT * 2).await;
                Ok(())
            })))
            .unwrap(),
    )
    .await;

    info!("Verifying that the response succeeds despite slow request time");
    time::timeout(TIMEOUT * 4, call)
        .await
        .expect("should succeed")
        .expect("should succed");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn idle_timeout_response_body() {
    let _trace = trace::test::trace_init();

    const TIMEOUT: Duration = Duration::from_secs(2);
    let (svc, mut handle) = mock(Timeouts {
        idle: Some(TIMEOUT),
        ..Default::default()
    });

    info!("Sending a request that is served immediately with a body that does not update");
    handle.allow(1);
    let call = send_req(svc.clone(), http_get());
    serve_req(
        &mut handle,
        http::Response::builder()
            .status(200)
            .body(http::BoxBody::new(MockBody::new(async move {
                futures::future::pending().await
            })))
            .unwrap(),
    )
    .await;

    info!("Verifying that the request body times out with the expected stream error");
    let mut rsp = call.await.unwrap().into_body();
    let error = time::timeout(TIMEOUT * 2, rsp.data())
        .await
        .expect("should timeout internally")
        .expect("should timeout internally")
        .err()
        .expect("should timeout internally");
    assert!(
        matches!(
            errors::cause_ref(error.as_ref()),
            Some(BodyTimeoutError::Idle(_)),
        ),
        "expected idle timeout, got {error:?}"
    );
}

// === Utils ===

type Handle = tower_test::mock::Handle<Request, Response>;

fn mock(timeouts: Timeouts) -> (svc::BoxCloneHttp, Handle) {
    let addr = SocketAddr::new([192, 0, 2, 41].into(), 1234);
    let dest: NameAddr = "example.com:1234".parse::<NameAddr>().unwrap();

    let (inner, handle) = tower_test::mock::pair();
    let connect = HttpConnect::default().service(addr, inner);
    let resolve = support::resolver().endpoint_exists(dest.clone(), addr, Default::default());
    let (rt, _shutdown) = runtime();
    let stack = Outbound::new(default_config(), rt, &mut Default::default())
        .with_stack(svc::ArcNewService::new(connect))
        .push_http_cached(resolve)
        .into_inner();

    let (tx, routes) = {
        let backend = default_backend(&dest);
        let route = mk_route(backend.clone(), timeouts);
        watch::channel(Routes::Policy(policy::Params::Http(policy::HttpParams {
            addr: dest.into(),
            meta: ParentRef(client_policy::Meta::new_default("parent")),
            backends: Arc::new([backend]),
            routes: Arc::new([route]),
            failure_accrual: client_policy::FailureAccrual::None,
        })))
    };
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

fn mk_route(backend: client_policy::Backend, timeouts: Timeouts) -> client_policy::http::Route {
    use client_policy::{
        http::{self, Filter, Policy, Route, Rule},
        Meta, RouteBackend, RouteDistribution,
    };
    use once_cell::sync::Lazy;
    static NO_FILTERS: Lazy<Arc<[Filter]>> = Lazy::new(|| Arc::new([]));
    Route {
        hosts: vec![],
        rules: vec![Rule {
            matches: vec![http::r#match::MatchRequest::default()],
            policy: Policy {
                meta: Meta::new_default("timeout-route"),
                filters: NO_FILTERS.clone(),
                params: http::RouteParams {
                    timeouts,
                    ..Default::default()
                },
                distribution: RouteDistribution::FirstAvailable(Arc::new([RouteBackend {
                    filters: NO_FILTERS.clone(),
                    backend,
                }])),
            },
        }],
    }
}
