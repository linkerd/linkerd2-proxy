use super::{policy, Routes};
use crate::test_util::*;
use crate::{BackendRef, Outbound, ParentRef, RouteRef};
use linkerd_app_core::{
    proxy::http::{self, BoxBody, HttpBody, StatusCode},
    svc::{self, NewService, ServiceExt},
    transport::addrs::*,
    Error, NameAddr, Result,
};
use linkerd_proxy_client_policy as client_policy;
use parking_lot::Mutex;
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};
use tokio::sync::watch;
use tracing::Instrument;

mod basic;
mod failure_accrual;
mod headers;
mod retries;
mod timeouts;

type Request = http::Request<http::BoxBody>;
type Response = http::Response<http::BoxBody>;

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

// === Utils ===

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

async fn mk_rsp(status: StatusCode, body: impl ToString) -> Result<Response> {
    Ok(http::Response::builder()
        .status(status)
        .body(http::BoxBody::new(body.to_string()))
        .unwrap())
}

async fn mk_grpc_rsp(code: tonic::Code) -> Result<Response> {
    Ok(http::Response::builder()
        .version(::http::Version::HTTP_2)
        .header("content-type", "application/grpc")
        .body(BoxBody::new(MockBody::trailers(async move {
            let mut trls = http::HeaderMap::default();
            trls.insert("grpc-status", (code as u8).to_string().parse().unwrap());
            Ok(Some(trls))
        })))
        .unwrap())
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

async fn serve(
    handle: &mut tower_test::mock::Handle<Request, Response>,
    call: impl Future<Output = Result<Response>> + Send + 'static,
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
        if !req.body().is_end_stream() {
            req.body_mut()
                .trailers()
                .await
                .expect("request body must not error");
        }
    }
    drop(req);

    tokio::spawn(
        async move {
            let res = call.await;
            tracing::debug!(?res, "Sending response");
            match res {
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
        meta: BackendRef(Meta::new_default("test")),
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
                meta: RouteRef(Meta::new_default("test_route")),
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

type Handle = tower_test::mock::Handle<Request, Response>;

fn mock_http(params: client_policy::http::RouteParams) -> (svc::BoxCloneHttp, Handle) {
    let dest = "example.com:1234".parse::<NameAddr>().unwrap();
    let backend = default_backend(&dest);
    let route = mk_route(backend.clone(), params);
    mock(policy::Params::Http(policy::HttpParams {
        addr: dest.into(),
        meta: ParentRef(client_policy::Meta::new_default("parent")),
        backends: Arc::new([backend]),
        routes: Arc::new([route]),
        failure_accrual: client_policy::FailureAccrual::None,
    }))
}

fn mock_grpc(params: client_policy::grpc::RouteParams) -> (svc::BoxCloneHttp, Handle) {
    let dest = "example.com:1234".parse::<NameAddr>().unwrap();
    let backend = default_backend(&dest);
    let route = mk_route(backend.clone(), params);
    mock(policy::Params::Grpc(policy::GrpcParams {
        addr: dest.into(),
        meta: ParentRef(client_policy::Meta::new_default("parent")),
        backends: Arc::new([backend]),
        routes: Arc::new([route]),
        failure_accrual: client_policy::FailureAccrual::None,
    }))
}

fn mock(params: policy::Params) -> (svc::BoxCloneHttp, Handle) {
    let (inner, handle) = tower_test::mock::pair();

    let addr = SocketAddr::new([192, 0, 2, 41].into(), 1234);
    let connect = HttpConnect::default().service(addr, inner);
    let resolve = support::resolver().endpoint_exists(
        params.addr().name_addr().unwrap().clone(),
        addr,
        Default::default(),
    );
    let (rt, shutdown) = runtime();
    let stack = Outbound::new(default_config(), rt, &mut Default::default())
        .with_stack(svc::ArcNewService::new(connect))
        .push_http_cached(resolve)
        .into_inner();

    let (tx, routes) = watch::channel(Routes::Policy(params));
    tokio::spawn(async move {
        tx.closed().await;
        drop(shutdown);
    });

    let svc = stack.new_service(Target {
        num: 1,
        version: http::Version::H2,
        routes,
    });

    (svc, handle)
}

fn mk_route<M: Default, F, P>(
    backend: client_policy::Backend,
    params: P,
) -> client_policy::route::Route<M, client_policy::RoutePolicy<F, P>> {
    use client_policy::*;

    route::Route {
        hosts: vec![],
        rules: vec![route::Rule {
            matches: vec![M::default()],
            policy: RoutePolicy {
                meta: RouteRef(Meta::new_default("route")),
                filters: [].into(),
                distribution: RouteDistribution::FirstAvailable(Arc::new([RouteBackend {
                    filters: [].into(),
                    backend,
                }])),
                params,
            },
        }],
    }
}
