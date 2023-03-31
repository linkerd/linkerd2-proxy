use super::*;
use crate::test_util::*;
use linkerd_app_core::{
    errors, exp_backoff::ExponentialBackoff, svc::NewService, svc::ServiceExt, trace, Error,
};
use linkerd_proxy_client_policy as client_policy;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::watch, time};
use tracing::Instrument;

const AUTHORITY: &str = "logical.test.svc.cluster.local";
const PORT: u16 = 666;

#[tokio::test(flavor = "current_thread")]
async fn routes() {
    let _trace = trace::test::trace_init();

    let addr = SocketAddr::new([192, 0, 2, 41].into(), PORT);
    let dest: NameAddr = format!("{AUTHORITY}:{PORT}")
        .parse::<NameAddr>()
        .expect("dest addr is valid");
    let (svc, mut handle) = tower_test::mock::pair();
    let connect = HttpConnect::default().service(addr, svc);
    let resolve = support::resolver().endpoint_exists(dest.clone(), addr, Default::default());
    let (rt, _shutdown) = runtime();
    let stack = Outbound::new(default_config(), rt)
        .with_stack(connect)
        .push_http_cached(resolve)
        .into_inner();

    let backend = default_backend(&dest);
    let (_route_tx, routes) =
        watch::channel(Routes::Policy(policy::Params::Http(policy::HttpParams {
            addr: dest.into(),
            backends: Arc::new([backend.clone()]),
            routes: Arc::new([default_route(backend)]),
            failure_accrual: client_policy::FailureAccrual::None,
        })));
    let target = Target {
        num: 1,
        version: http::Version::H2,
        routes,
    };
    let svc = stack.new_service(target);

    handle.allow(1);
    let rsp = send_req(svc.clone(), http::Request::get("/"));
    serve_req(&mut handle, http::Response::builder().status(200)).await;
    assert_eq!(
        rsp.await.expect("request must succeed").status(),
        http::StatusCode::OK
    );
}

#[tokio::test(flavor = "current_thread")]
async fn consecutive_failures_accrue() {
    let _trace = trace::test::with_default_filter(format!(
        "{},linkerd_app_outbound=trace",
        trace::test::DEFAULT_LOG
    ));

    let addr = SocketAddr::new([192, 0, 2, 41].into(), PORT);
    let dest: NameAddr = format!("{AUTHORITY}:{PORT}")
        .parse::<NameAddr>()
        .expect("dest addr is valid");
    let (svc, mut handle) = tower_test::mock::pair();
    let connect = HttpConnect::default().service(addr, svc);
    let resolve = support::resolver().endpoint_exists(dest.clone(), addr, Default::default());
    let (rt, _shutdown) = runtime();
    let stack = Outbound::new(default_config(), rt)
        .with_stack(connect)
        .push_http_cached(resolve)
        .into_inner();

    let backend = default_backend(&dest);
    let min_backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(60);
    let (_route_tx, routes) =
        watch::channel(Routes::Policy(policy::Params::Http(policy::HttpParams {
            addr: dest.into(),
            backends: Arc::new([backend.clone()]),
            routes: Arc::new([default_route(backend)]),
            failure_accrual: client_policy::FailureAccrual::ConsecutiveFailures {
                max_failures: 3,
                backoff: ExponentialBackoff::try_new(
                    min_backoff,
                    max_backoff,
                    // no jitter --- ensure the test is deterministic
                    0.0,
                )
                .unwrap(),
            },
        })));
    let target = Target {
        num: 1,
        version: http::Version::H2,
        routes,
    };
    let svc = stack.new_service(target);

    handle.allow(1);
    let rsp = send_req(svc.clone(), http::Request::get("/"));
    serve_req(&mut handle, http::Response::builder().status(200)).await;
    assert_eq!(
        rsp.await.expect("request must succeed").status(),
        http::StatusCode::OK
    );

    // fail 3 requests so that we hit the consecutive failures accrual limit
    for _ in 0..3 {
        handle.allow(1);
        let rsp = send_req(svc.clone(), http::Request::get("/"));
        serve_req(
            &mut handle,
            http::Response::builder().status(http::StatusCode::INTERNAL_SERVER_ERROR),
        )
        .await;
        assert_eq!(
            rsp.await.expect("request should succeed with 500").status(),
            http::StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    // Ensure that the error is because of the breaker, and not because the
    // underlying service doesn't poll ready.
    handle.allow(1);
    // We are now in failfast.
    let error = send_req(svc.clone(), http::Request::get("/"))
        .await
        .expect_err("service should be in failfast");
    assert!(
        errors::is_caused_by::<errors::FailFastError>(error.as_ref()),
        "service should be in failfast"
    );

    let error = send_req(svc.clone(), http::Request::get("/"))
        .await
        .expect_err("service should be in failfast");
    assert!(
        errors::is_caused_by::<errors::LoadShedError>(error.as_ref()),
        "service should be in failfast"
    );

    // After the probation period, a subsequent request should be failed by
    // hitting the service.
    time::sleep(min_backoff + Duration::from_secs(1)).await;
    tracing::info!("After probation");
    tokio::task::yield_now().await;

    let rsp = send_req(svc.clone(), http::Request::get("/"));
    serve_req(
        &mut handle,
        http::Response::builder().status(http::StatusCode::INTERNAL_SERVER_ERROR),
    )
    .await;
    assert_eq!(
        rsp.await.expect("request should succeed with 500").status(),
        http::StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[derive(Clone, Debug)]
struct Target {
    num: usize,
    version: http::Version,
    routes: watch::Receiver<Routes>,
}

type MockSvc = tower_test::mock::Mock<http::Request<http::BoxBody>, http::Response<http::BoxBody>>;

#[derive(Clone, Default)]
struct HttpConnect {
    svcs: Arc<Mutex<HashMap<SocketAddr, MockSvc>>>,
}

// === impl Target ===

impl PartialEq for Target {
    fn eq(&self, other: &Self) -> bool {
        self.num == other.num
    }
}

impl Eq for Target {}

impl std::hash::Hash for Target {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.num.hash(state);
    }
}

impl svc::Param<http::Version> for Target {
    fn param(&self) -> http::Version {
        self.version
    }
}

impl svc::Param<watch::Receiver<Routes>> for Target {
    fn param(&self) -> watch::Receiver<Routes> {
        self.routes.clone()
    }
}

// === impl HttpConnect ===

impl HttpConnect {
    fn service(self, addr: SocketAddr, svc: MockSvc) -> Self {
        self.svcs.lock().unwrap().insert(addr, svc);
        self
    }
}

impl<T: svc::Param<Remote<ServerAddr>>> svc::NewService<T> for HttpConnect {
    type Service = MockSvc;
    fn new_service(&self, target: T) -> Self::Service {
        let Remote(ServerAddr(addr)) = target.param();
        self.svcs
            .lock()
            .unwrap()
            .get(&addr)
            .expect("tried to connect to an unexpected address")
            .clone()
    }
}

#[track_caller]
fn send_req(
    svc: impl svc::Service<
            http::Request<http::BoxBody>,
            Response = http::Response<http::BoxBody>,
            Error = Error,
            Future = impl Send + 'static,
        > + Send
        + 'static,
    builder: ::http::request::Builder,
) -> impl Future<Output = Result<http::Response<http::BoxBody>, Error>> + Send + 'static {
    let mut req = builder.body(http::BoxBody::default()).unwrap();
    let span = tracing::info_span!(
        "send_req",
        "{} {} {:?}",
        req.method(),
        req.uri(),
        req.version(),
    );
    let rsp = tokio::spawn(
        async move {
            tracing::info!("sending request...");
            // the HTTP stack will panic if a request is missing a client handle
            let (client_handle, _) =
                http::ClientHandle::new(SocketAddr::new([10, 0, 0, 42].into(), 42069));
            req.extensions_mut().insert(client_handle);
            let rsp = svc.oneshot(req);
            let rsp = time::timeout(Duration::from_secs(60), rsp)
                .await
                .expect("request should complete in a timely manner");
            tracing::info!(?rsp);
            rsp
        }
        .instrument(span),
    );
    async move { rsp.await.expect("request task must not panic") }
}

async fn serve_req(
    handle: &mut tower_test::mock::Handle<
        http::Request<http::BoxBody>,
        http::Response<http::BoxBody>,
    >,
    rsp: ::http::response::Builder,
) {
    let (req, send_rsp) = time::timeout(Duration::from_secs(60), handle.next_request())
        .await
        .expect("service must receive request in a timely manner")
        .expect("service must receive request");
    tracing::info!(?req, "received request");
    send_rsp.send_response(rsp.body(http::BoxBody::default()).unwrap());
    tracing::info!(?req, "response sent");
}

fn default_backend(path: impl ToString) -> client_policy::Backend {
    use client_policy::{
        Backend, BackendDispatcher, EndpointDiscovery, Load, Meta, PeakEwma, Queue,
    };
    Backend {
        meta: Meta::new_default("test"),
        queue: Queue {
            capacity: 100,
            failfast_timeout: Duration::from_secs(10),
        },
        dispatcher: BackendDispatcher::BalanceP2c(
            Load::PeakEwma(PeakEwma {
                decay: Duration::from_secs(10),
                default_rtt: Duration::from_millis(30),
            }),
            EndpointDiscovery::DestinationGet {
                path: path.to_string(),
            },
        ),
    }
}

fn default_route(backend: client_policy::Backend) -> client_policy::http::Route {
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
                meta: Meta::new_default("test_route"),
                filters: NO_FILTERS.clone(),
                failure_policy: Default::default(),
                distribution: RouteDistribution::FirstAvailable(Arc::new([RouteBackend {
                    filters: NO_FILTERS.clone(),
                    backend: backend.clone(),
                }])),
            },
        }],
    }
}
