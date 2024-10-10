use super::{
    super::{Backend, Grpc, Http},
    RouteBackendMetrics,
};
use crate::http::{
    concrete,
    logical::{
        policy::route::metrics::{
            labels, test_util::*, LabelGrpcRouteBackendRsp, LabelHttpRouteBackendRsp,
        },
        Concrete,
    },
};
use linkerd_app_core::{
    svc::{self, http::BoxBody, Layer, NewService},
    transport::{Remote, ServerAddr},
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
                .body(BoxBody::new(MockBody::new(async {
                    Err("a spooky ghost".into())
                })))
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
                    .body(BoxBody::new(MockBody::trailers(async move {
                        let mut trailers = http::HeaderMap::new();
                        trailers.insert("grpc-status", http::HeaderValue::from_static("0"));
                        Ok(Some(trailers))
                    })))
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
                    .body(BoxBody::new(MockBody::trailers(async move {
                        let mut trailers = http::HeaderMap::new();
                        trailers.insert("grpc-status", http::HeaderValue::from_static("5"));
                        Ok(Some(trailers))
                    })))
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
                    .body(BoxBody::new(MockBody::new(async {
                        Err("a spooky ghost".into())
                    })))
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
