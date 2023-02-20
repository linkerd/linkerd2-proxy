use super::*;
use linkerd_app_core::{
    svc::NewService,
    svc::{Layer, ServiceExt},
    trace,
};
use tokio::time;

#[tokio::test(flavor = "current_thread")]
async fn header_based_route() {
    let _trace = trace::test::trace_init();

    let default_addr = ([127, 0, 0, 1], 8081).into();
    let special_addr = ([127, 0, 0, 1], 8082).into();
    let default = policy::Backend {
        meta: policy::Meta::new_default("default"),
        queue: policy::Queue {
            capacity: 10,
            failfast_timeout: time::Duration::from_secs(1),
        },
        dispatcher: policy::BackendDispatcher::Forward(default_addr, Default::default()),
    };
    let special = policy::Backend {
        meta: policy::Meta::new_default("special"),
        queue: policy::Queue {
            capacity: 10,
            failfast_timeout: time::Duration::from_secs(1),
        },
        dispatcher: policy::BackendDispatcher::Forward(special_addr, Default::default()),
    };

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

    let http = HttpRoutes {
        addr: Addr::Socket(([127, 0, 0, 1], 8080).into()),
        routes: Arc::new([route]),
        backends: std::iter::once(default).chain(Some(special)).collect(),
    };

    let (default, mut default_handle) = tower_test::mock::pair();
    let (special, mut special_handle) = tower_test::mock::pair();
    let router = layer()
        .layer(move |concrete: Concrete<()>| {
            if let concrete::Dispatch::Forward(Remote(ServerAddr(addr)), ..) = concrete.target {
                if addr == default_addr {
                    return default.clone();
                }
                if addr == special_addr {
                    return special.clone();
                }
            }
            panic!("unexpected target: {:?}", concrete.target);
        })
        .new_service(Params::from((Routes::Http(http), ())));

    default_handle.allow(2);
    special_handle.allow(2);

    let special_fut = router.clone().oneshot(
        http::Request::builder()
            .header("x-special", "true")
            .body(http::BoxBody::default())
            .unwrap(),
    );
    tokio::select! {
        _ = default_handle.next_request() => panic!("unexpected request"),
        _ = special_handle.next_request() => {}
        _ = special_fut => panic!("unexpected response"),
        _ = time::sleep(time::Duration::from_secs(1)) => panic!("timed out"),
    }

    let default_fut = router.oneshot(
        http::Request::builder()
            .body(http::BoxBody::default())
            .unwrap(),
    );
    tokio::select! {
        _ = default_handle.next_request() => {}
        _ = special_handle.next_request() => panic!("unexpected request"),
        _ = default_fut => panic!("unexpected response"),
        _ = time::sleep(time::Duration::from_secs(1)) => panic!("timed out"),
    }
}
