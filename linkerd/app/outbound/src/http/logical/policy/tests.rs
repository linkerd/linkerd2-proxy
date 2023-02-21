use super::*;
use linkerd_app_core::{
    svc::NewService,
    svc::{Layer, ServiceExt},
    trace,
};
use std::net::SocketAddr;
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

    let default_addr = ([127, 0, 0, 1], 8081).into();
    let special_addr = ([127, 0, 0, 1], 8082).into();

    let http = {
        let default = mk_backend("default", default_addr);
        let special = mk_backend("special", special_addr);
        let route = policy::http::Route {
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
                    policy: policy::RoutePolicy {
                        meta: policy::Meta::new_default("special"),
                        filters: Arc::new([]),
                        distribution: policy::RouteDistribution::FirstAvailable(Arc::new([
                            policy::RouteBackend {
                                filters: Arc::new([]),
                                backend: special.clone(),
                            },
                        ])),
                    },
                },
                policy::http::Rule {
                    matches: vec![route::http::MatchRequest::default()],
                    policy: policy::RoutePolicy {
                        meta: policy::Meta::new_default("default"),
                        filters: Arc::new([]),
                        distribution: policy::RouteDistribution::FirstAvailable(Arc::new([
                            policy::RouteBackend {
                                filters: Arc::new([]),
                                backend: default.clone(),
                            },
                        ])),
                    },
                },
            ],
        };

        HttpRoutes {
            addr: Addr::Socket(([127, 0, 0, 1], 8080).into()),
            routes: Arc::new([route]),
            backends: std::iter::once(default).chain(Some(special)).collect(),
        }
    };

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

    let router = layer()
        .layer(inner)
        .new_service(Params::from((Routes::Http(http), ())));
    default.allow(2);
    special.allow(2);

    let req = http::Request::builder()
        .body(http::BoxBody::default())
        .unwrap();
    tokio::select! {
        _ = router.clone().oneshot(req) => panic!("unexpected response"),
        _ = default.next_request() => {}
        _ = special.next_request() => panic!("unexpected request"),
        _ = time::sleep(time::Duration::from_secs(1)) => panic!("timed out"),
    }

    let req = http::Request::builder()
        .header("x-special", "true")
        .body(http::BoxBody::default())
        .unwrap();
    tokio::select! {
        _ = router.oneshot(req) => panic!("unexpected response"),
        _ = default.next_request() => panic!("unexpected request"),
        _ = special.next_request() => {}
        _ = time::sleep(time::Duration::from_secs(1)) => panic!("timed out"),
    }
}
