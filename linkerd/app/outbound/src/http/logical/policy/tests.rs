use super::{super::concrete, *};
use crate::{BackendRef, ParentRef, RouteRef};
use linkerd_app_core::{
    svc::NewService,
    svc::{Layer, ServiceExt},
    trace,
};
use linkerd_http_route as route;
use linkerd_proxy_client_policy as policy;
use std::{num::NonZeroU16, sync::Arc};
use tokio::time;

#[tokio::test(flavor = "current_thread")]
async fn header_based_route() {
    let _trace = trace::test::trace_init();

    let mk_backend = |name: &'static str| policy::Backend {
        meta: BackendRef(Arc::new(policy::Meta::Resource {
            group: "core".into(),
            kind: "Service".into(),
            namespace: "ns".into(),
            name: name.into(),
            port: NonZeroU16::new(8080),
            section: None,
        })),
        queue: policy::Queue {
            capacity: 10,
            failfast_timeout: time::Duration::from_secs(1),
        },
        dispatcher: policy::BackendDispatcher::BalanceP2c(
            policy::Load::PeakEwma(policy::PeakEwma {
                decay: time::Duration::from_secs(10),
                default_rtt: time::Duration::from_millis(300),
            }),
            policy::EndpointDiscovery::DestinationGet {
                path: format!("{name}.ns.svc.cluster.local:8080"),
            },
        ),
    };
    let mk_policy = |name: &'static str, backend: policy::Backend| policy::RoutePolicy {
        meta: RouteRef(Arc::new(policy::Meta::Resource {
            group: "policy.linkerd.io".into(),
            kind: "HTTPRoute".into(),
            namespace: "ns".into(),
            name: name.into(),
            port: None,
            section: None,
        })),
        filters: Arc::new([]),
        params: Default::default(),
        distribution: policy::RouteDistribution::FirstAvailable(Arc::new([policy::RouteBackend {
            filters: Arc::new([]),
            backend,
        }])),
    };

    // Stack that produces mock services.
    let (inner_default, mut default) = tower_test::mock::pair();
    let (inner_special, mut special) = tower_test::mock::pair();
    let inner = move |concrete: Concrete<()>| {
        if let concrete::Dispatch::Balance(ref addr, ..) = concrete.target {
            if addr
                .name()
                .eq_ignore_ascii_case("default.ns.svc.cluster.local")
            {
                return inner_default.clone();
            }
            if addr
                .name()
                .eq_ignore_ascii_case("special.ns.svc.cluster.local")
            {
                return inner_special.clone();
            }
        }
        panic!("unexpected target: {:?}", concrete.target);
    };

    let parent_ref = ParentRef(Arc::new(policy::Meta::Resource {
        group: "core".into(),
        kind: "Service".into(),
        namespace: "ns".into(),
        name: "papa".into(),
        port: NonZeroU16::new(7979),
        section: None,
    }));

    let default_backend = mk_backend("default");
    let default_backend_ref = default_backend.meta.clone();
    let default_policy = mk_policy("default", default_backend.clone());
    let default_route_ref = default_policy.meta.clone();

    let special_backend = mk_backend("special");
    let special_policy = mk_policy("special", special_backend.clone());
    let special_route_ref = special_policy.meta.clone();
    let special_backend_ref = special_backend.meta.clone();

    // Routes that configure a special header-based route and a default route.
    let routes = Params::Http(router::HttpParams {
        addr: Addr::Socket(([127, 0, 0, 1], 8080).into()),
        meta: parent_ref.clone(),
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
                    policy: special_policy.clone(),
                },
                policy::http::Rule {
                    matches: vec![route::http::MatchRequest::default()],
                    policy: default_policy.clone(),
                },
            ],
        }]),
        backends: std::iter::once(default_backend.clone())
            .chain(Some(special_backend.clone()))
            .collect(),
        failure_accrual: Default::default(),
    });

    let metrics = RouteMetrics::default();
    let router = Policy::layer(metrics.clone(), Default::default())
        .layer(inner)
        .new_service(Policy::from((routes, ())));

    let default_reqs = metrics.backend_request_count(
        parent_ref.clone(),
        default_route_ref.clone(),
        default_backend_ref.clone(),
    );
    let special_reqs = metrics.backend_request_count(
        parent_ref.clone(),
        special_route_ref.clone(),
        special_backend_ref.clone(),
    );
    assert_eq!(default_reqs.get(), 0);
    assert_eq!(special_reqs.get(), 0);

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
    assert_eq!(default_reqs.get(), 1);
    assert_eq!(special_reqs.get(), 0);

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
    assert_eq!(default_reqs.get(), 1);
    assert_eq!(special_reqs.get(), 1);

    // Hold the router to prevent inner services from being dropped.
    drop(router);
}

#[tokio::test(flavor = "current_thread")]
async fn http_filter_request_headers() {
    let _trace = trace::test::trace_init();

    let addr = ([127, 0, 0, 1], 18080).into();
    let backend = policy::Backend {
        meta: BackendRef(policy::Meta::new_default("test")),
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
            meta: ParentRef(policy::Meta::new_default("splinter")),
            routes: Arc::new([policy::http::Route {
                hosts: Default::default(),
                rules: vec![policy::http::Rule {
                    matches: vec![route::http::MatchRequest::default()],
                    policy: policy::RoutePolicy {
                        meta: RouteRef(policy::Meta::new_default("turtles")),
                        params: Default::default(),
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

    let router = Policy::layer(Default::default(), Default::default())
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
