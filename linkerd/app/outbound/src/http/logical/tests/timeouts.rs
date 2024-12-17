use super::{super::LogicalError, *};
use linkerd_app_core::{
    errors,
    proxy::http::{
        self,
        stream_timeouts::{BodyTimeoutError, ResponseTimeoutError},
        Body, BoxBody,
    },
    trace,
};
use linkerd_proxy_client_policy::{self as client_policy, http::Timeouts};
use tokio::time;
use tracing::{info, Instrument};

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn request_timeout_response_headers() {
    let _trace = trace::test::trace_init();

    const TIMEOUT: time::Duration = time::Duration::from_secs(2);
    let (svc, mut handle) = mock_http(client_policy::http::RouteParams {
        timeouts: Timeouts {
            request: Some(TIMEOUT),
            ..Default::default()
        },
        ..Default::default()
    });

    info!("Sending a request that does not respond within the timeout");
    handle.allow(1);
    let call = send_req(svc.clone(), http_get());
    serve(&mut handle, async move {
        time::sleep(TIMEOUT * 2).await;
        Ok(http::Response::builder()
            .status(204)
            .body(http::BoxBody::default())
            .unwrap())
    })
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

    const TIMEOUT: time::Duration = time::Duration::from_secs(2);
    let (svc, mut handle) = mock_http(client_policy::http::RouteParams {
        timeouts: Timeouts {
            request: Some(TIMEOUT),
            ..Default::default()
        },
        ..Default::default()
    });

    info!("Sending a request that does not respond within the timeout");
    handle.allow(1);
    let call = send_req(
        svc.clone(),
        http::Request::builder()
            .method("POST")
            .body(BoxBody::new(MockBody::pending()))
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

    const TIMEOUT: time::Duration = time::Duration::from_secs(2);
    let (svc, mut handle) = mock_http(client_policy::http::RouteParams {
        timeouts: Timeouts {
            request: Some(TIMEOUT),
            ..Default::default()
        },
        ..Default::default()
    });

    info!("Sending a request that responds immediately but does not complete");
    handle.allow(1);
    let call = send_req(svc.clone(), http_get());
    serve(
        &mut handle,
        future::ok(
            http::Response::builder()
                .status(200)
                .body(BoxBody::new(MockBody::pending()))
                .unwrap(),
        ),
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

    const TIMEOUT: time::Duration = time::Duration::from_secs(2);
    let (svc, mut handle) = mock_http(client_policy::http::RouteParams {
        timeouts: Timeouts {
            response: Some(TIMEOUT),
            ..Default::default()
        },
        ..Default::default()
    });

    info!("Sending a request that does not respond within the timeout");
    handle.allow(1);
    let call = send_req(svc.clone(), http_get());
    serve(&mut handle, async move {
        time::sleep(TIMEOUT * 2).await;
        Ok(http::Response::builder()
            .status(204)
            .body(http::BoxBody::default())
            .unwrap())
    })
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

    const TIMEOUT: time::Duration = time::Duration::from_secs(2);
    let (svc, mut handle) = mock_http(client_policy::http::RouteParams {
        timeouts: Timeouts {
            response: Some(TIMEOUT),
            ..Default::default()
        },
        ..Default::default()
    });

    tokio::spawn(
        async move {
            handle.allow(1);
            serve(&mut handle, async move {
                info!("Serving a response that never completes");
                Ok(http::Response::builder()
                    .status(200)
                    .body(http::BoxBody::new(MockBody::pending()))
                    .unwrap())
            })
            .await;
        }
        .in_current_span(),
    );

    info!("Sending a request that responds immediately but does not complete");
    let mut rsp = send_req(svc.clone(), http_get()).await.unwrap().into_body();

    info!("Verifying that the request body times out with the expected stream error");
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

    const TIMEOUT: time::Duration = time::Duration::from_secs(2);
    let (svc, mut handle) = mock_http(client_policy::http::RouteParams {
        timeouts: Timeouts {
            response: Some(TIMEOUT),
            ..Default::default()
        },
        ..Default::default()
    });

    info!("Sending a request that exceeds the response timeout");
    handle.allow(1);
    let call = send_req(svc.clone(), http_get());
    serve(&mut handle, async move {
        info!("Serving a response that never completes");
        Ok(http::Response::builder()
            .status(200)
            .body(http::BoxBody::new(MockBody::pending()))
            .unwrap())
    })
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

    const TIMEOUT: time::Duration = time::Duration::from_secs(2);
    let (svc, mut handle) = mock_http(client_policy::http::RouteParams {
        timeouts: Timeouts {
            idle: Some(TIMEOUT),
            ..Default::default()
        },
        ..Default::default()
    });

    info!("Sending a request that is served immediately with a body that does not update");
    handle.allow(1);
    let call = send_req(svc.clone(), http_get());
    serve(&mut handle, async move {
        info!("Serving a response that never completes");
        Ok(http::Response::builder()
            .status(200)
            .body(http::BoxBody::new(MockBody::pending()))
            .unwrap())
    })
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
