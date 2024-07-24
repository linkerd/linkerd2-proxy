use super::{policy, Outbound, ParentRef, Routes};
use crate::test_util::*;
use linkerd_app_core::{
    errors,
    exp_backoff::ExponentialBackoff,
    proxy::http::{self, BoxBody, HttpBody, StatusCode},
    svc::{self, NewService, ServiceExt},
    trace,
    transport::addrs::*,
    Error, NameAddr, Result,
};
use linkerd_proxy_client_policy as client_policy;
use parking_lot::Mutex;
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{sync::watch, task, time};
use tracing::{info, Instrument};

mod retries;
mod timeouts;

const AUTHORITY: &str = "logical.test.svc.cluster.local";
const PORT: u16 = 666;

type Request = http::Request<http::BoxBody>;
type Response = http::Response<http::BoxBody>;

#[tokio::test(flavor = "current_thread", start_paused = true)]
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
    let stack = Outbound::new(default_config(), rt, &mut Default::default())
        .with_stack(svc::ArcNewService::new(connect))
        .push_http_cached(resolve)
        .into_inner();

    let backend = default_backend(&dest);
    let (_route_tx, routes) =
        watch::channel(Routes::Policy(policy::Params::Http(policy::HttpParams {
            addr: dest.into(),
            meta: ParentRef(client_policy::Meta::new_default("parent")),
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
    let rsp = send_req(svc.clone(), http_get());
    serve_req(&mut handle, mk_rsp(StatusCode::OK, "good")).await;
    assert_eq!(
        rsp.await.expect("request must succeed").status(),
        http::StatusCode::OK
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn consecutive_failures_accrue() {
    let _trace = trace::test::with_default_filter(format!("{},trace", trace::test::DEFAULT_LOG));

    let addr = SocketAddr::new([192, 0, 2, 41].into(), PORT);
    let dest: NameAddr = format!("{AUTHORITY}:{PORT}")
        .parse::<NameAddr>()
        .expect("dest addr is valid");
    let (svc, mut handle) = tower_test::mock::pair();
    let connect = HttpConnect::default().service(addr, svc);
    let resolve = support::resolver().endpoint_exists(dest.clone(), addr, Default::default());
    let (rt, _shutdown) = runtime();
    let cfg = default_config();
    let stack = Outbound::new(cfg.clone(), rt, &mut Default::default())
        .with_stack(svc::ArcNewService::new(connect))
        .push_http_cached(resolve)
        .into_inner();

    let backend = default_backend(&dest);
    // Ensure that the probe delay is longer than the failfast timeout, so that
    // the service is only probed after it has entered failfast when the gate
    // shuts.
    let min_backoff = cfg.http_request_queue.failfast_timeout + Duration::from_secs(1);
    let backoff = ExponentialBackoff::try_new(
        min_backoff,
        min_backoff * 6,
        // no jitter --- ensure the test is deterministic
        0.0,
    )
    .unwrap();
    let mut backoffs = backoff.stream();
    let (_route_tx, routes) =
        watch::channel(Routes::Policy(policy::Params::Http(policy::HttpParams {
            addr: dest.into(),
            meta: ParentRef(client_policy::Meta::new_default("parent")),
            backends: Arc::new([backend.clone()]),
            routes: Arc::new([default_route(backend)]),
            failure_accrual: client_policy::FailureAccrual::ConsecutiveFailures {
                max_failures: 3,
                backoff,
            },
        })));
    let target = Target {
        num: 1,
        version: http::Version::H2,
        routes,
    };
    let svc = stack.new_service(target);

    info!("Sending good request");
    handle.allow(1);
    let rsp = send_req(svc.clone(), http_get());
    serve_req(&mut handle, mk_rsp(StatusCode::OK, "good")).await;
    assert_rsp(rsp, StatusCode::OK, "good").await;

    // fail 3 requests so that we hit the consecutive failures accrual limit
    for i in 1..=3 {
        info!("Sending bad request {i}/3");
        handle.allow(1);
        let rsp = send_req(svc.clone(), http_get());
        serve_req(
            &mut handle,
            mk_rsp(StatusCode::INTERNAL_SERVER_ERROR, "bad"),
        )
        .await;
        assert_rsp(rsp, StatusCode::INTERNAL_SERVER_ERROR, "bad").await;
    }

    // Ensure that the error is because of the breaker, and not because the
    // underlying service doesn't poll ready.
    info!("Sending request while in failfast");
    handle.allow(1);
    // We are now in failfast.
    let error = send_req(svc.clone(), http_get())
        .await
        .expect_err("service should be in failfast");
    assert!(
        errors::is_caused_by::<errors::FailFastError>(error.as_ref()),
        "service should be in failfast"
    );

    info!("Sending request while in loadshed");
    let error = send_req(svc.clone(), http_get())
        .await
        .expect_err("service should be in failfast");
    assert!(
        errors::is_caused_by::<errors::LoadShedError>(error.as_ref()),
        "service should be in failfast"
    );

    // After the probation period, a subsequent request should be failed by
    // hitting the service.
    info!("Waiting for probation");
    backoffs.next().await;
    task::yield_now().await;

    info!("Sending a bad request while in probation");
    handle.allow(1);
    let rsp = send_req(svc.clone(), http_get());
    info!("Serving response");
    tokio::time::timeout(
        time::Duration::from_secs(10),
        serve_req(
            &mut handle,
            mk_rsp(StatusCode::INTERNAL_SERVER_ERROR, "bad"),
        ),
    )
    .await
    .expect("no timeouts");
    assert_rsp(rsp, StatusCode::INTERNAL_SERVER_ERROR, "bad").await;

    // We are now in failfast.
    info!("Sending a failfast request while the circuit is broken");
    handle.allow(1);
    let error = send_req(svc.clone(), http_get())
        .await
        .expect_err("service should be in failfast");
    assert!(
        errors::is_caused_by::<errors::FailFastError>(error.as_ref()),
        "service should be in failfast"
    );

    // Wait out the probation period again
    info!("Waiting for probation again");
    backoffs.next().await;
    task::yield_now().await;

    // The probe request succeeds
    info!("Sending a good request while in probation");
    handle.allow(1);
    let rsp = send_req(svc.clone(), http_get());
    tokio::time::timeout(
        time::Duration::from_secs(10),
        serve_req(&mut handle, mk_rsp(StatusCode::OK, "good")),
    )
    .await
    .expect("no timeouts");
    assert_rsp(rsp, StatusCode::OK, "good").await;

    // The gate is now open again
    info!("Sending a final good request");
    handle.allow(1);
    let rsp = send_req(svc.clone(), http_get());
    tokio::time::timeout(
        time::Duration::from_secs(10),
        serve_req(&mut handle, mk_rsp(StatusCode::OK, "good")),
    )
    .await
    .expect("no timeouts");
    assert_rsp(rsp, StatusCode::OK, "good").await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn balancer_doesnt_select_tripped_breakers() {
    let _trace = trace::test::with_default_filter(format!(
        "{},linkerd_app_outbound=trace,linkerd_stack=trace,linkerd2_proxy_http_balance=trace",
        trace::test::DEFAULT_LOG
    ));

    let addr1 = SocketAddr::new([192, 0, 2, 41].into(), PORT);
    let addr2 = SocketAddr::new([192, 0, 2, 42].into(), PORT);
    let dest: NameAddr = format!("{AUTHORITY}:{PORT}")
        .parse::<NameAddr>()
        .expect("dest addr is valid");
    let (svc1, mut handle1) = tower_test::mock::pair();
    let (svc2, mut handle2) = tower_test::mock::pair();
    let connect = HttpConnect::default()
        .service(addr1, svc1)
        .service(addr2, svc2);
    let resolve = support::resolver();
    let mut dest_tx = resolve.endpoint_tx(dest.clone());
    dest_tx
        .add([(addr1, Default::default()), (addr2, Default::default())])
        .unwrap();
    let (rt, _shutdown) = runtime();
    let cfg = default_config();
    let stack = Outbound::new(cfg.clone(), rt, &mut Default::default())
        .with_stack(svc::ArcNewService::new(connect))
        .push_http_cached(resolve)
        .into_inner();

    let backend = default_backend(&dest);
    // Ensure that the probe delay is longer than the failfast timeout, so that
    // the service is only probed after it has entered failfast when the gate
    // shuts.
    let min_backoff = cfg.http_request_queue.failfast_timeout + Duration::from_secs(1);
    let backoff = ExponentialBackoff::try_new(
        min_backoff,
        min_backoff * 6,
        // no jitter --- ensure the test is deterministic
        0.0,
    )
    .unwrap();
    let (_route_tx, routes) =
        watch::channel(Routes::Policy(policy::Params::Http(policy::HttpParams {
            addr: dest.into(),
            meta: ParentRef(client_policy::Meta::new_default("parent")),
            backends: Arc::new([backend.clone()]),
            routes: Arc::new([default_route(backend)]),
            failure_accrual: client_policy::FailureAccrual::ConsecutiveFailures {
                max_failures: 3,
                backoff,
            },
        })));
    let target = Target {
        num: 1,
        version: http::Version::H2,
        routes,
    };
    let svc = stack.new_service(target);

    // fail 3 requests so that we hit the consecutive failures accrual limit
    let mut failed = 0;
    while failed < 3 {
        handle1.allow(1);
        handle2.allow(1);
        info!(failed);
        let rsp = send_req(svc.clone(), http_get());
        let (expected_status, expected_body) = tokio::select! {
            _ = serve_req(&mut handle1, mk_rsp(StatusCode::OK, "endpoint 1")) => {
                info!("Balancer selected good endpoint");
                (StatusCode::OK, "endpoint 1")
            }
            _ = serve_req(&mut handle2, mk_rsp(StatusCode::INTERNAL_SERVER_ERROR, "endpoint 2")) => {
                info!("Balancer selected bad endpoint");
                failed += 1;
                (StatusCode::INTERNAL_SERVER_ERROR, "endpoint 2")
            }
        };
        assert_rsp(rsp, expected_status, expected_body).await;
        task::yield_now().await;
    }

    handle1.allow(1);
    handle2.allow(1);
    let rsp = send_req(svc.clone(), http_get());
    // The load balancer will select endpoint 1, because endpoint 2 isn't ready.
    serve_req(&mut handle1, mk_rsp(StatusCode::OK, "endpoint 1")).await;
    assert_rsp(rsp, StatusCode::OK, "endpoint 1").await;

    // The load balancer should continue selecting the non-failing endpoint.
    for _ in 0..8 {
        handle1.allow(1);
        handle2.allow(1);
        let rsp = send_req(svc.clone(), http_get());
        serve_req(&mut handle1, mk_rsp(StatusCode::OK, "endpoint 1")).await;
        assert_rsp(rsp, StatusCode::OK, "endpoint 1").await;
    }
}

// === Utils ===

#[derive(Clone, Debug)]
struct Target {
    num: usize,
    version: http::Version,
    routes: watch::Receiver<Routes>,
}

type MockSvc = tower_test::mock::Mock<Request, Response>;

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
        self.svcs.lock().insert(addr, svc);
        self
    }
}

impl<T: svc::Param<Remote<ServerAddr>>> svc::NewService<T> for HttpConnect {
    type Service = svc::BoxHttp;
    fn new_service(&self, target: T) -> Self::Service {
        let Remote(ServerAddr(addr)) = target.param();
        svc::BoxHttp::new(
            self.svcs
                .lock()
                .get(&addr)
                .expect("tried to connect to an unexpected address")
                .clone(),
        )
    }
}

// ===

#[track_caller]
fn send_req(
    svc: impl svc::Service<Request, Response = Response, Error = Error, Future = impl Send + 'static>
        + Send
        + 'static,
    mut req: ::http::Request<BoxBody>,
) -> impl Future<Output = Result<Response>> + Send + 'static {
    let span = tracing::info_span!(
        "send_req",
        "{} {} {:?}",
        req.method(),
        req.uri(),
        req.version(),
    );
    let rsp = tokio::spawn(
        async move {
            // the HTTP stack will panic if a request is missing a client handle
            let (client_handle, _) =
                http::ClientHandle::new(SocketAddr::new([10, 0, 0, 42].into(), 42069));
            req.extensions_mut().insert(client_handle);
            tracing::debug!("Sending");
            let rsp = svc.oneshot(req).await;
            tracing::debug!(?rsp, "Response");
            rsp
        }
        .instrument(span),
    );
    async move { rsp.await.expect("request task must not panic") }
}

fn mk_rsp(status: StatusCode, body: impl ToString) -> Response {
    http::Response::builder()
        .status(status)
        .body(http::BoxBody::new(body.to_string()))
        .unwrap()
}

fn mk_grpc_rsp(code: tonic::Code) -> Response {
    http::Response::builder()
        .version(::http::Version::HTTP_2)
        .header(
            "content-type",
            http::HeaderValue::from_static("application/grpc"),
        )
        .body(BoxBody::new(MockBody::trailers(async move {
            let mut trls = http::HeaderMap::default();
            trls.insert("grpc-status", (code as u8).to_string().parse().unwrap());
            Ok(Some(trls))
        })))
        .unwrap()
}

async fn assert_rsp<T: std::fmt::Debug>(
    rsp: impl Future<Output = Result<Response>>,
    status: StatusCode,
    expected_body: T,
) where
    bytes::Bytes: PartialEq<T>,
{
    let rsp = rsp.await.expect("response must not fail");
    assert_eq!(rsp.status(), status, "expected status code to be {status}");
    let body = hyper::body::to_bytes(rsp.into_body())
        .await
        .expect("body must not fail");
    assert_eq!(body, expected_body, "expected body to be {expected_body:?}");
}

async fn serve_req(handle: &mut tower_test::mock::Handle<Request, Response>, rsp: Response) {
    serve_delayed(Duration::ZERO, handle, Ok(rsp)).await;
}

async fn serve_delayed(
    delay: Duration,
    handle: &mut tower_test::mock::Handle<Request, Response>,
    rsp: Result<Response>,
) {
    let (mut req, tx) = handle
        .next_request()
        .await
        .expect("service must receive request");
    tracing::debug!(?req, "Received request");

    // Ensure the whole request is processed.
    if !req.body().is_end_stream() {
        while let Some(res) = req.body_mut().data().await {
            res.expect("request body must not error");
        }
    }
    if !req.body().is_end_stream() {
        req.body_mut()
            .trailers()
            .await
            .expect("request body must not error");
    }
    drop(req);

    tokio::spawn(
        async move {
            if delay > Duration::ZERO {
                tracing::debug!(?delay, "Sleeping");
                tokio::time::sleep(delay).await;
            }

            tracing::debug!(?rsp, "Sending response");
            match rsp {
                Ok(rsp) => tx.send_response(rsp),
                Err(e) => tx.send_error(e),
            }
        }
        .in_current_span(),
    );
}

fn http_get() -> http::Request<BoxBody> {
    http::Request::get("/").body(Default::default()).unwrap()
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
                params: http::RouteParams::default(),
                distribution: RouteDistribution::FirstAvailable(Arc::new([RouteBackend {
                    filters: NO_FILTERS.clone(),
                    backend,
                }])),
            },
        }],
    }
}
