use super::{
    super::{Grpc, Http, Route},
    labels,
    test_util::*,
    LabelGrpcRouteRsp, LabelHttpRouteRsp, RequestMetrics,
};
use linkerd_app_core::svc::{self, http::BoxBody, Layer, NewService};
use linkerd_proxy_client_policy as policy;

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn http_request_statuses() {
    let _trace = linkerd_tracing::test::trace_init();

    let metrics = super::HttpRouteMetrics::default().requests;
    let parent_ref = crate::ParentRef(policy::Meta::new_default("parent"));
    let route_ref = crate::RouteRef(policy::Meta::new_default("route"));
    let (mut svc, mut handle) = mock_http_route_metrics(&metrics, &parent_ref, &route_ref);

    // Send one request and ensure it's counted.
    let ok = metrics.get_statuses(&labels::Rsp(
        labels::HttpRoute(parent_ref.clone(), route_ref.clone(), None),
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

    // Send another request and ensure it's counted with a different response
    // status.
    let no_content = metrics.get_statuses(&labels::Rsp(
        labels::HttpRoute(parent_ref.clone(), route_ref.clone(), None),
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

    // Emit a response with an error and ensure it's counted.
    let unknown = metrics.get_statuses(&labels::Rsp(
        labels::HttpRoute(parent_ref.clone(), route_ref.clone(), None),
        labels::HttpRsp {
            status: None,
            error: Some(labels::Error::Unknown),
        },
    ));
    send_assert_incremented(&unknown, &mut handle, &mut svc, Default::default(), |tx| {
        tx.send_error("a spooky ghost")
    })
    .await;

    // Emit a successful response with a body that fails and ensure that both
    // the status and error are recorded.
    let mixed = metrics.get_statuses(&labels::Rsp(
        labels::HttpRoute(parent_ref, route_ref, None),
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

    assert_eq!(unknown.get(), 1);
    assert_eq!(ok.get(), 1);
    assert_eq!(no_content.get(), 1);
    assert_eq!(mixed.get(), 1);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn http_request_hostnames() {
    const HOST_1: &str = "great.website";
    const URI_1_1: &str = "https://great.website/path/to/index.html#fragment";
    const URI_1_2: &str = "https://great.website/another/index.html";
    const HOST_2: &str = "different.website";
    const URI_2: &str = "https://different.website/index.html";

    let _trace = linkerd_tracing::test::trace_init();

    let metrics = super::HttpRouteMetrics::default().requests;
    let parent_ref = crate::ParentRef(policy::Meta::new_default("parent"));
    let route_ref = crate::RouteRef(policy::Meta::new_default("route"));
    let (mut svc, mut handle) = mock_http_route_metrics(&metrics, &parent_ref, &route_ref);

    let get_counter = |host: &str, status: Option<http::StatusCode>| {
        metrics.get_statuses(&labels::Rsp(
            labels::HttpRoute(parent_ref.clone(), route_ref.clone(), Some(host.to_owned())),
            labels::HttpRsp {
                status,
                error: None,
            },
        ))
    };

    let host1_ok = get_counter(HOST_1, Some(http::StatusCode::OK));
    let host1_teapot = get_counter(HOST_1, Some(http::StatusCode::IM_A_TEAPOT));
    let host2_ok = get_counter(HOST_2, Some(http::StatusCode::OK));

    // Send one request and ensure it's counted.
    send_assert_incremented(
        &host1_ok,
        &mut handle,
        &mut svc,
        http::Request::builder()
            .uri(URI_1_1)
            .body(BoxBody::default())
            .unwrap(),
        |tx| {
            tx.send_response(
                http::Response::builder()
                    .status(200)
                    .body(BoxBody::default())
                    .unwrap(),
            )
        },
    )
    .await;
    assert_eq!(host1_ok.get(), 1);
    assert_eq!(host1_teapot.get(), 0);
    assert_eq!(host2_ok.get(), 0);

    // Send another request to a different path on the same host.
    send_assert_incremented(
        &host1_teapot,
        &mut handle,
        &mut svc,
        http::Request::builder()
            .uri(URI_1_2)
            .body(BoxBody::default())
            .unwrap(),
        |tx| {
            tx.send_response(
                http::Response::builder()
                    .status(418)
                    .body(BoxBody::default())
                    .unwrap(),
            )
        },
    )
    .await;
    assert_eq!(host1_ok.get(), 1);
    assert_eq!(host1_teapot.get(), 1);
    assert_eq!(host2_ok.get(), 0);

    // Send a request to a different host.
    send_assert_incremented(
        &host2_ok,
        &mut handle,
        &mut svc,
        http::Request::builder()
            .uri(URI_2)
            .body(BoxBody::default())
            .unwrap(),
        |tx| {
            tx.send_response(
                http::Response::builder()
                    .status(200)
                    .body(BoxBody::default())
                    .unwrap(),
            )
        },
    )
    .await;
    assert_eq!(host1_ok.get(), 1);
    assert_eq!(host1_teapot.get(), 1);
    assert_eq!(host2_ok.get(), 1);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn grpc_request_statuses_ok() {
    let _trace = linkerd_tracing::test::trace_init();

    let metrics = super::GrpcRouteMetrics::default().requests;
    let parent_ref = crate::ParentRef(policy::Meta::new_default("parent"));
    let route_ref = crate::RouteRef(policy::Meta::new_default("route"));
    let (mut svc, mut handle) = mock_grpc_route_metrics(&metrics, &parent_ref, &route_ref);

    // Send one request and ensure it's counted.
    let ok = metrics.get_statuses(&labels::Rsp(
        labels::GrpcRoute(parent_ref.clone(), route_ref.clone()),
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
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn grpc_request_statuses_not_found() {
    let _trace = linkerd_tracing::test::trace_init();

    let metrics = super::GrpcRouteMetrics::default().requests;
    let parent_ref = crate::ParentRef(policy::Meta::new_default("parent"));
    let route_ref = crate::RouteRef(policy::Meta::new_default("route"));
    let (mut svc, mut handle) = mock_grpc_route_metrics(&metrics, &parent_ref, &route_ref);

    // Send another request and ensure it's counted with a different response
    // status.
    let not_found = metrics.get_statuses(&labels::Rsp(
        labels::GrpcRoute(parent_ref.clone(), route_ref.clone()),
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

    let metrics = super::GrpcRouteMetrics::default().requests;
    let parent_ref = crate::ParentRef(policy::Meta::new_default("parent"));
    let route_ref = crate::RouteRef(policy::Meta::new_default("route"));
    let (mut svc, mut handle) = mock_grpc_route_metrics(&metrics, &parent_ref, &route_ref);

    let unknown = metrics.get_statuses(&labels::Rsp(
        labels::GrpcRoute(parent_ref.clone(), route_ref.clone()),
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

    let metrics = super::GrpcRouteMetrics::default().requests;
    let parent_ref = crate::ParentRef(policy::Meta::new_default("parent"));
    let route_ref = crate::RouteRef(policy::Meta::new_default("route"));
    let (mut svc, mut handle) = mock_grpc_route_metrics(&metrics, &parent_ref, &route_ref);

    let unknown = metrics.get_statuses(&labels::Rsp(
        labels::GrpcRoute(parent_ref.clone(), route_ref.clone()),
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

// === Utils ===

pub fn mock_http_route_metrics(
    metrics: &RequestMetrics<LabelHttpRouteRsp>,
    parent_ref: &crate::ParentRef,
    route_ref: &crate::RouteRef,
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
                    params: policy::http::RouteParams::default(),
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
            params: Route {
                parent: (),
                addr: std::net::SocketAddr::new([0, 0, 0, 0].into(), 8080).into(),
                parent_ref: parent_ref.clone(),
                route_ref: route_ref.clone(),
                filters: [].into(),
                distribution: Default::default(),
                params: policy::http::RouteParams::default(),
            },
        });

    (svc::BoxHttp::new(svc), handle)
}

pub fn mock_grpc_route_metrics(
    metrics: &RequestMetrics<LabelGrpcRouteRsp>,
    parent_ref: &crate::ParentRef,
    route_ref: &crate::RouteRef,
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
                    params: policy::grpc::RouteParams::default(),
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
            params: Route {
                parent: (),
                addr: std::net::SocketAddr::new([0, 0, 0, 0].into(), 8080).into(),
                parent_ref: parent_ref.clone(),
                route_ref: route_ref.clone(),
                filters: [].into(),
                distribution: Default::default(),
                params: policy::grpc::RouteParams::default(),
            },
        });

    (svc::BoxHttp::new(svc), handle)
}
