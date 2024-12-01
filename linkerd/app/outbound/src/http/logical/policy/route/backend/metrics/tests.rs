use super::{
    super::{Backend, Grpc, Http},
    labels,
    test_util::*,
    LabelGrpcRouteBackendRsp, LabelHttpRouteBackendRsp, RouteBackendMetrics,
};
use crate::http::{concrete, logical::Concrete};
use bytes::Buf;
use linkerd_app_core::{
    svc::{self, http::BoxBody, Layer, NewService},
    transport::{Remote, ServerAddr},
    Error,
};
use linkerd_proxy_client_policy as policy;

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn http_request_statuses() {
    let _trace = linkerd_tracing::test::trace_init();

    let metrics = super::RouteBackendMetrics::default();
    let parent_ref = crate::ParentRef(policy::Meta::new_default("parent"));
    let route_ref = crate::RouteRef(policy::Meta::new_default("route"));
    let backend_ref = crate::BackendRef(policy::Meta::new_default("backend"));
    let (mut svc, mut handle) =
        mock_http_route_backend_metrics(&metrics, &parent_ref, &route_ref, &backend_ref);

    let route_backend =
        labels::RouteBackend(parent_ref.clone(), route_ref.clone(), backend_ref.clone());

    let requests =
        metrics.backend_request_count(parent_ref.clone(), route_ref.clone(), backend_ref.clone());
    assert_eq!(requests.get(), 0);

    // Send one request and ensure it's counted.
    let ok = metrics.get_statuses(&labels::Rsp(
        route_backend.clone(),
        labels::HttpRsp {
            status: Some(http::StatusCode::OK),
            error: None,
        },
    ));
    send_assert_incremented(&ok, &mut handle, &mut svc, Default::default(), |tx| {
        tx.send_response(
            http::Response::builder()
                .status(200)
                .body(BoxBody::default())
                .unwrap(),
        )
    })
    .await;
    assert_eq!(requests.get(), 1);

    // Send another request and ensure it's counted with a different response
    // status.
    let no_content = metrics.get_statuses(&labels::Rsp(
        route_backend.clone(),
        labels::HttpRsp {
            status: Some(http::StatusCode::NO_CONTENT),
            error: None,
        },
    ));
    send_assert_incremented(
        &no_content,
        &mut handle,
        &mut svc,
        Default::default(),
        |tx| {
            tx.send_response(
                http::Response::builder()
                    .status(204)
                    .body(BoxBody::default())
                    .unwrap(),
            )
        },
    )
    .await;
    assert_eq!(requests.get(), 2);

    // Emit a response with an error and ensure it's counted.
    let unknown = metrics.get_statuses(&labels::Rsp(
        route_backend.clone(),
        labels::HttpRsp {
            status: None,
            error: Some(labels::Error::Unknown),
        },
    ));
    send_assert_incremented(&unknown, &mut handle, &mut svc, Default::default(), |tx| {
        tx.send_error("a spooky ghost")
    })
    .await;
    assert_eq!(requests.get(), 3);

    // Emit a successful response with a body that fails and ensure that both
    // the status and error are recorded.
    let mixed = metrics.get_statuses(&labels::Rsp(
        route_backend.clone(),
        labels::HttpRsp {
            status: Some(http::StatusCode::OK),
            error: Some(labels::Error::Unknown),
        },
    ));
    send_assert_incremented(&mixed, &mut handle, &mut svc, Default::default(), |tx| {
        tx.send_response(
            http::Response::builder()
                .status(200)
                .body(BoxBody::new(MockBody::error("a spooky ghost")))
                .unwrap(),
        )
    })
    .await;
    assert_eq!(requests.get(), 4);

    assert_eq!(unknown.get(), 1);
    assert_eq!(ok.get(), 1);
    assert_eq!(no_content.get(), 1);
    assert_eq!(mixed.get(), 1);
}

/// Tests that metrics count frames in the backend response body.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn body_data_layer_records_frames() -> Result<(), Error> {
    use http_body::Body;
    use linkerd_app_core::proxy::http;
    use linkerd_http_prom::body_data::response::BodyDataMetrics;
    use tower::{Service, ServiceExt};

    let _trace = linkerd_tracing::test::trace_init();

    let metrics = super::RouteBackendMetrics::default();
    let parent_ref = crate::ParentRef(policy::Meta::new_default("parent"));
    let route_ref = crate::RouteRef(policy::Meta::new_default("route"));
    let backend_ref = crate::BackendRef(policy::Meta::new_default("backend"));

    let (mut svc, mut handle) =
        mock_http_route_backend_metrics(&metrics, &parent_ref, &route_ref, &backend_ref);
    handle.allow(1);

    // Create a request.
    let req = {
        let empty = hyper::Body::empty();
        let body = BoxBody::new(empty);
        http::Request::builder().method("DOOT").body(body).unwrap()
    };

    // Call the service once it is ready to accept a request.
    tracing::info!("calling service");
    svc.ready().await.expect("ready");
    let call = svc.call(req);
    let (req, send_resp) = handle.next_request().await.unwrap();
    debug_assert_eq!(req.method().as_str(), "DOOT");

    // Acquire the counters for this backend.
    tracing::info!("acquiring response body metrics");
    let labels = labels::RouteBackend(parent_ref.clone(), route_ref.clone(), backend_ref.clone());
    let BodyDataMetrics {
        // TODO(kate): currently, histograms do not expose their observation count or sum. so,
        // we're left unable to exercise these metrics until prometheus/client_rust#242 lands.
        //   - https://github.com/prometheus/client_rust/pull/241
        //   - https://github.com/prometheus/client_rust/pull/242
        #[cfg(feature = "prometheus-client-rust-242")]
        frame_size,
        ..
    } = metrics.get_response_body_metrics(&labels);

    // Before we've sent a response, the counter should be zero.
    #[cfg(feature = "prometheus-client-rust-242")]
    {
        assert_eq!(frame_size.count(), 0);
        assert_eq!(frame_size.sum(), 0);
    }

    // Create a response whose body is backed by a channel that we can send chunks to, send it.
    tracing::info!("sending response");
    let mut resp_tx = {
        #[allow(deprecated, reason = "linkerd/linkerd2#8733")]
        let (tx, body) = hyper::Body::channel();
        let body = BoxBody::new(body);
        let resp = http::Response::builder()
            .status(http::StatusCode::IM_A_TEAPOT)
            .body(body)
            .unwrap();
        send_resp.send_response(resp);
        tx
    };

    // Before we've sent any bytes, the counter should be zero.
    #[cfg(feature = "prometheus-client-rust-242")]
    {
        assert_eq!(frame_size.count(), 0);
        assert_eq!(frame_size.sum(), 0);
    }

    // On the client end, poll our call future and await the response.
    tracing::info!("polling service future");
    let (parts, body) = call.await?.into_parts();
    debug_assert_eq!(parts.status, 418);

    let mut body = Box::pin(body);

    /// Returns the next chunk from a boxed body.
    async fn read_chunk(body: &mut std::pin::Pin<Box<BoxBody>>) -> Result<Vec<u8>, Error> {
        use std::task::{Context, Poll};
        let mut ctx = Context::from_waker(futures_util::task::noop_waker_ref());
        let data = match body.as_mut().poll_data(&mut ctx) {
            Poll::Ready(Some(Ok(d))) => d,
            _ => panic!("next chunk should be ready"),
        };
        let chunk = data.chunk().to_vec();
        Ok(chunk)
    }

    {
        // Send a chunk, confirm that our counters are incremented.
        tracing::info!("sending first chunk");
        resp_tx.send_data("hello".into()).await?;
        let chunk = read_chunk(&mut body).await?;
        debug_assert_eq!("hello".as_bytes(), chunk, "should get same value back out");
        #[cfg(feature = "prometheus-client-rust-242")]
        assert_eq!(frame_size.count(), 1);
        #[cfg(feature = "prometheus-client-rust-242")]
        assert_eq!(frame_size.sum(), 5);
    }

    {
        // Send another chunk, confirm that our counters are incremented once more.
        tracing::info!("sending second chunk");
        resp_tx.send_data(", world!".into()).await?;
        let chunk = read_chunk(&mut body).await?;
        debug_assert_eq!(
            ", world!".as_bytes(),
            chunk,
            "should get same value back out"
        );
        #[cfg(feature = "prometheus-client-rust-242")]
        assert_eq!(frame_size.count(), 2);
        #[cfg(feature = "prometheus-client-rust-242")]
        assert_eq!(frame_size.sum(), 5 + 8);
    }

    {
        // Close the body, show that the counters remain at the same values.
        use std::task::{Context, Poll};
        tracing::info!("closing response body");
        drop(resp_tx);
        let mut ctx = Context::from_waker(futures_util::task::noop_waker_ref());
        match body.as_mut().poll_data(&mut ctx) {
            Poll::Ready(None) => {}
            _ => panic!("got unexpected poll result"),
        };
        #[cfg(feature = "prometheus-client-rust-242")]
        assert_eq!(frame_size.count(), 2);
        #[cfg(feature = "prometheus-client-rust-242")]
        assert_eq!(frame_size.sum(), 5 + 8);
    }

    Ok(())
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn grpc_request_statuses_ok() {
    let _trace = linkerd_tracing::test::trace_init();

    let metrics = super::RouteBackendMetrics::default();
    let parent_ref = crate::ParentRef(policy::Meta::new_default("parent"));
    let route_ref = crate::RouteRef(policy::Meta::new_default("route"));
    let backend_ref = crate::BackendRef(policy::Meta::new_default("backend"));
    let (mut svc, mut handle) =
        mock_grpc_route_backend_metrics(&metrics, &parent_ref, &route_ref, &backend_ref);

    let requests =
        metrics.backend_request_count(parent_ref.clone(), route_ref.clone(), backend_ref.clone());
    assert_eq!(requests.get(), 0);

    let ok = metrics.get_statuses(&labels::Rsp(
        labels::RouteBackend(parent_ref.clone(), route_ref.clone(), backend_ref.clone()),
        labels::GrpcRsp {
            status: Some(tonic::Code::Ok),
            error: None,
        },
    ));
    send_assert_incremented(
        &ok,
        &mut handle,
        &mut svc,
        http::Request::builder()
            .method("POST")
            .uri("http://host/svc/method")
            .body(Default::default())
            .unwrap(),
        |tx| {
            tx.send_response(
                http::Response::builder()
                    .body(BoxBody::new(MockBody::grpc_status(0)))
                    .unwrap(),
            )
        },
    )
    .await;
    assert_eq!(requests.get(), 1);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn grpc_request_statuses_not_found() {
    let _trace = linkerd_tracing::test::trace_init();

    let metrics = super::RouteBackendMetrics::default();
    let parent_ref = crate::ParentRef(policy::Meta::new_default("parent"));
    let route_ref = crate::RouteRef(policy::Meta::new_default("route"));
    let backend_ref = crate::BackendRef(policy::Meta::new_default("backend"));
    let (mut svc, mut handle) =
        mock_grpc_route_backend_metrics(&metrics, &parent_ref, &route_ref, &backend_ref);

    let not_found = metrics.get_statuses(&labels::Rsp(
        labels::RouteBackend(parent_ref.clone(), route_ref.clone(), backend_ref.clone()),
        labels::GrpcRsp {
            status: Some(tonic::Code::NotFound),
            error: None,
        },
    ));
    send_assert_incremented(
        &not_found,
        &mut handle,
        &mut svc,
        http::Request::builder()
            .method("POST")
            .uri("http://host/svc/method")
            .body(Default::default())
            .unwrap(),
        |tx| {
            tx.send_response(
                http::Response::builder()
                    .body(BoxBody::new(MockBody::grpc_status(5)))
                    .unwrap(),
            )
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn grpc_request_statuses_error_response() {
    let _trace = linkerd_tracing::test::trace_init();

    let metrics = super::RouteBackendMetrics::default();
    let parent_ref = crate::ParentRef(policy::Meta::new_default("parent"));
    let route_ref = crate::RouteRef(policy::Meta::new_default("route"));
    let backend_ref = crate::BackendRef(policy::Meta::new_default("backend"));
    let (mut svc, mut handle) =
        mock_grpc_route_backend_metrics(&metrics, &parent_ref, &route_ref, &backend_ref);

    let unknown = metrics.get_statuses(&labels::Rsp(
        labels::RouteBackend(parent_ref.clone(), route_ref.clone(), backend_ref.clone()),
        labels::GrpcRsp {
            status: None,
            error: Some(labels::Error::Unknown),
        },
    ));
    send_assert_incremented(
        &unknown,
        &mut handle,
        &mut svc,
        http::Request::builder()
            .method("POST")
            .uri("http://host/svc/method")
            .body(Default::default())
            .unwrap(),
        |tx| tx.send_error("a spooky ghost"),
    )
    .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn grpc_request_statuses_error_body() {
    let _trace = linkerd_tracing::test::trace_init();

    let metrics = super::RouteBackendMetrics::default();
    let parent_ref = crate::ParentRef(policy::Meta::new_default("parent"));
    let route_ref = crate::RouteRef(policy::Meta::new_default("route"));
    let backend_ref = crate::BackendRef(policy::Meta::new_default("backend"));
    let (mut svc, mut handle) =
        mock_grpc_route_backend_metrics(&metrics, &parent_ref, &route_ref, &backend_ref);

    let unknown = metrics.get_statuses(&labels::Rsp(
        labels::RouteBackend(parent_ref.clone(), route_ref.clone(), backend_ref.clone()),
        labels::GrpcRsp {
            status: None,
            error: Some(labels::Error::Unknown),
        },
    ));
    send_assert_incremented(
        &unknown,
        &mut handle,
        &mut svc,
        http::Request::builder()
            .method("POST")
            .uri("http://host/svc/method")
            .body(Default::default())
            .unwrap(),
        |tx| {
            tx.send_response(
                http::Response::builder()
                    .body(BoxBody::new(MockBody::error("a spooky ghost")))
                    .unwrap(),
            )
        },
    )
    .await;
}

// === Util ===

fn mock_http_route_backend_metrics(
    metrics: &RouteBackendMetrics<LabelHttpRouteBackendRsp>,
    parent_ref: &crate::ParentRef,
    route_ref: &crate::RouteRef,
    backend_ref: &crate::BackendRef,
) -> (svc::BoxHttp, Handle) {
    let req = http::Request::builder().body(()).unwrap();
    let (r#match, _) = policy::route::find(
        &[policy::http::Route {
            hosts: vec![],
            rules: vec![policy::route::Rule {
                matches: vec![policy::http::r#match::MatchRequest::default()],
                policy: policy::http::Policy {
                    meta: route_ref.0.clone(),
                    filters: [].into(),
                    distribution: policy::RouteDistribution::Empty,
                    params: Default::default(),
                },
            }],
        }],
        &req,
    )
    .expect("find default route");

    let (tx, handle) = tower_test::mock::pair::<http::Request<BoxBody>, http::Response<BoxBody>>();
    let svc = super::layer(metrics)
        .layer(move |_t: Http<()>| tx.clone())
        .new_service(Http {
            r#match,
            params: Backend {
                route_ref: route_ref.clone(),
                filters: [].into(),
                concrete: Concrete {
                    target: concrete::Dispatch::Forward(
                        Remote(ServerAddr(std::net::SocketAddr::new(
                            [0, 0, 0, 0].into(),
                            8080,
                        ))),
                        Default::default(),
                    ),
                    authority: None,
                    failure_accrual: Default::default(),
                    parent: (),
                    parent_ref: parent_ref.clone(),
                    backend_ref: backend_ref.clone(),
                },
            },
        });

    (svc::BoxHttp::new(svc), handle)
}

fn mock_grpc_route_backend_metrics(
    metrics: &RouteBackendMetrics<LabelGrpcRouteBackendRsp>,
    parent_ref: &crate::ParentRef,
    route_ref: &crate::RouteRef,
    backend_ref: &crate::BackendRef,
) -> (svc::BoxHttp, Handle) {
    let req = http::Request::builder()
        .method("POST")
        .uri("http://host/svc/method")
        .body(())
        .unwrap();
    let (r#match, _) = policy::route::find(
        &[policy::grpc::Route {
            hosts: vec![],
            rules: vec![policy::route::Rule {
                matches: vec![policy::grpc::r#match::MatchRoute::default()],
                policy: policy::grpc::Policy {
                    meta: route_ref.0.clone(),
                    filters: [].into(),
                    distribution: policy::RouteDistribution::Empty,
                    params: Default::default(),
                },
            }],
        }],
        &req,
    )
    .expect("find default route");

    let (tx, handle) = tower_test::mock::pair::<http::Request<BoxBody>, http::Response<BoxBody>>();
    let svc = super::layer(metrics)
        .layer(move |_t: Grpc<()>| tx.clone())
        .new_service(Grpc {
            r#match,
            params: Backend {
                route_ref: route_ref.clone(),
                filters: [].into(),
                concrete: Concrete {
                    target: concrete::Dispatch::Forward(
                        Remote(ServerAddr(std::net::SocketAddr::new(
                            [0, 0, 0, 0].into(),
                            8080,
                        ))),
                        Default::default(),
                    ),
                    authority: None,
                    failure_accrual: Default::default(),
                    parent: (),
                    parent_ref: parent_ref.clone(),
                    backend_ref: backend_ref.clone(),
                },
            },
        });

    (svc::BoxHttp::new(svc), handle)
}
