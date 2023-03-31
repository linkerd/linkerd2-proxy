use super::{super::concrete, *};
use linkerd_app_core::{
    svc::NewService,
    svc::{Layer, ServiceExt},
    trace,
    transport::addrs::*,
};
use linkerd_http_route as route;
use linkerd_proxy_client_policy as policy;
use std::{net::SocketAddr, sync::Arc};
use tokio::time;

#[tokio::test(flavor = "current_thread")]
async fn header_based_route() {
    let _trace = trace::test::trace_init();

    let mk_backend = |name: &'static str, addr: SocketAddr| policy::Backend {
        meta: policy::Meta::new_default(name),
        queue: policy::Queue {
            capacity: 10,
            failfast_timeout: time::Duration::from_secs(1),
        },
        dispatcher: policy::BackendDispatcher::Forward(addr, Default::default()),
    };
    let mk_policy = |name: &'static str, backend: policy::Backend| policy::RoutePolicy {
        meta: policy::Meta::new_default(name),
        filters: Arc::new([]),
        failure_policy: Default::default(),
        distribution: policy::RouteDistribution::FirstAvailable(Arc::new([policy::RouteBackend {
            filters: Arc::new([]),
            backend,
        }])),
    };

    // Stack that produces mock services.
    let default_addr = ([127, 0, 0, 1], 18080).into();
    let special_addr = ([127, 0, 0, 1], 28080).into();
    let (inner_default, mut default) = tower_test::mock::pair();
    let (inner_special, mut special) = tower_test::mock::pair();
    let inner = move |concrete: Concrete<()>| {
        if let concrete::Dispatch::Forward(Remote(ServerAddr(addr)), ..) = concrete.target {
            if addr == default_addr {
                return inner_default.clone();
            }
            if addr == special_addr {
                return inner_special.clone();
            }
        }
        panic!("unexpected target: {:?}", concrete.target);
    };

    // Routes that configure a special header-based route and a default route.
    let routes = Params::Http({
        let default = mk_backend("default", default_addr);
        let special = mk_backend("special", special_addr);
        router::HttpParams {
            addr: Addr::Socket(([127, 0, 0, 1], 8080).into()),
            meta: policy::Meta::new_default("parent"),
            routes: Arc::new([policy::http::Route {
                hosts: Default::default(),
                rules: vec![
                    policy::http::Rule {
                        matches: vec![route::http::MatchRequest {
                            headers: vec![route::http::r#match::MatchHeader::Exact(
                                "x-special".parse().unwrap(),
                                "true".parse().unwrap(),
                            )],
                            ..Default::default()
                        }],
                        policy: mk_policy("special", special.clone()),
                    },
                    policy::http::Rule {
                        matches: vec![route::http::MatchRequest::default()],
                        policy: mk_policy("default", default.clone()),
                    },
                ],
            }]),
            backends: std::iter::once(default).chain(Some(special)).collect(),
            failure_accrual: Default::default(),
        }
    });

    let metrics = RouteBackendMetrics::default();
    let router = Policy::layer(metrics.clone())
        .layer(inner)
        .new_service(Policy::from((routes, ())));

    default.allow(1);
    special.allow(1);
    let req = http::Request::builder()
        .body(http::BoxBody::default())
        .unwrap();
    let _ = tokio::select! {
        biased;
        _ = router.clone().oneshot(req) => panic!("unexpected response"),
        _ = special.next_request() => panic!("unexpected request to special service"),
        _ = time::sleep(time::Duration::from_secs(1)) => panic!("timed out"),
        reqrsp = default.next_request() => reqrsp.expect("request"),
    };

    default.allow(1);
    special.allow(1);
    let req = http::Request::builder()
        .header("x-special", "true")
        .body(http::BoxBody::default())
        .unwrap();
    let _ = tokio::select! {
        biased;
        _ = router.clone().oneshot(req) => panic!("unexpected response"),
        _ = default.next_request() => panic!("unexpected request to default service"),
        _ = time::sleep(time::Duration::from_secs(1)) => panic!("timed out"),
        reqrsp = special.next_request() => reqrsp.expect("request"),
    };

    // Hold the router to prevent inner services from being dropped.
    drop(router);

    let report = linkerd_app_core::metrics::FmtMetrics::as_display(&metrics).to_string();
    let mut lines = report
        .lines()
        .filter(|l| !l.starts_with('#'))
        .collect::<Vec<_>>();
    lines.sort();
    assert_eq!(
        lines,
        vec![
            r#"outbound_http_route_backend_requests_total{parent_group="",parent_kind="default",parent_namespace="",parent_name="parent",route_group="",route_kind="default",route_namespace="",route_name="default",backend_group="",backend_kind="default",backend_namespace="",backend_name="default"} 1"#,
            r#"outbound_http_route_backend_requests_total{parent_group="",parent_kind="default",parent_namespace="",parent_name="parent",route_group="",route_kind="default",route_namespace="",route_name="special",backend_group="",backend_kind="default",backend_namespace="",backend_name="special"} 1"#,
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn http_filter_request_headers() {
    let _trace = trace::test::trace_init();

    let addr = ([127, 0, 0, 1], 18080).into();
    let backend = policy::Backend {
        meta: policy::Meta::new_default("test"),
        queue: policy::Queue {
            capacity: 10,
            failfast_timeout: time::Duration::from_secs(1),
        },
        dispatcher: policy::BackendDispatcher::Forward(addr, Default::default()),
    };

    // Stack that produces mock services.
    let (inner, mut handle) = tower_test::mock::pair();
    let inner = move |_: Concrete<()>| inner.clone();

    // Routes that configure a special header-based route and a default route.
    static PIZZA: http::HeaderName = http::HeaderName::from_static("pizza");
    static PARTY: http::HeaderValue = http::HeaderValue::from_static("party");
    static TUBULAR: http::HeaderValue = http::HeaderValue::from_static("tubular");
    static COWABUNGA: http::HeaderValue = http::HeaderValue::from_static("cowabunga");
    let routes = Params::Http({
        router::HttpParams {
            addr: Addr::Socket(([127, 0, 0, 1], 8080).into()),
            meta: policy::Meta::new_default("splinter"),
            routes: Arc::new([policy::http::Route {
                hosts: Default::default(),
                rules: vec![policy::http::Rule {
                    matches: vec![route::http::MatchRequest::default()],
                    policy: policy::RoutePolicy {
                        meta: policy::Meta::new_default("turtles"),
                        failure_policy: Default::default(),
                        filters: Arc::new([policy::http::Filter::RequestHeaders(
                            policy::http::filter::ModifyHeader {
                                add: vec![(PIZZA.clone(), TUBULAR.clone())],
                                ..Default::default()
                            },
                        )]),
                        distribution: policy::RouteDistribution::FirstAvailable(Arc::new([
                            policy::RouteBackend {
                                backend: backend.clone(),
                                filters: Arc::new([policy::http::Filter::RequestHeaders(
                                    policy::http::filter::ModifyHeader {
                                        add: vec![(PIZZA.clone(), COWABUNGA.clone())],
                                        ..Default::default()
                                    },
                                )]),
                            },
                        ])),
                    },
                }],
            }]),
            backends: std::iter::once(backend).collect(),
            failure_accrual: Default::default(),
        }
    });

    let router = Policy::layer(Default::default())
        .layer(inner)
        .new_service(Policy::from((routes, ())));

    handle.allow(1);
    let req = http::Request::builder()
        .header(&PIZZA, &PARTY)
        .body(http::BoxBody::default())
        .unwrap();
    let (req, _rsp) = tokio::select! {
        biased;
        _ = router.clone().oneshot(req) => panic!("unexpected response"),
        _ = time::sleep(time::Duration::from_secs(1)) => panic!("timed out"),
        reqrsp = handle.next_request() => reqrsp.expect("request"),
    };

    assert_eq!(
        req.headers().get_all(&PIZZA).iter().collect::<Vec<_>>(),
        vec![&PARTY, &TUBULAR, &COWABUNGA],
    );

    // Hold the router to prevent inner services from being dropped.
    drop(router);
}
