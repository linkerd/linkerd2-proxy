use super::*;
use linkerd_app_core::{
    svc::NewService,
    svc::{Layer, ServiceExt},
    trace,
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

    let default_addr = ([127, 0, 0, 1], 18080).into();
    let special_addr = ([127, 0, 0, 1], 28080).into();

    // Stack that produces mock services.
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
    let routes = Routes::Http({
        let default = mk_backend("default", default_addr);
        let special = mk_backend("special", special_addr);
        HttpRoutes {
            addr: Addr::Socket(([127, 0, 0, 1], 8080).into()),
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
        }
    });

    let router = Params::layer()
        .layer(inner)
        .new_service(Params::from((routes, ())));

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
}

fn mk_policy<F>(name: &'static str, backend: policy::Backend) -> policy::RoutePolicy<F> {
    policy::RoutePolicy {
        meta: policy::Meta::new_default(name),
        filters: Arc::new([]),
        distribution: policy::RouteDistribution::FirstAvailable(Arc::new([policy::RouteBackend {
            filters: Arc::new([]),
            backend,
        }])),
    }
}
