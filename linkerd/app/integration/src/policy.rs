use super::*;
pub use api::{inbound, outbound};
use api::{inbound::inbound_server_policies_server, outbound::outbound_policies_server};
use futures::stream;
use linkerd2_proxy_api as api;
use parking_lot::Mutex;
use std::{collections::VecDeque, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tonic as grpc;

#[derive(Debug, Default)]
pub struct Controller {
    outbound: Inner<outbound::TrafficSpec, outbound::OutboundPolicy>,
    inbound: Inner<u16, inbound::Server>,
}

#[derive(Debug, Clone)]
struct Server<Req, Rsp>(Arc<Inner<Req, Rsp>>);

#[derive(Debug)]
struct Inner<Req, Rsp> {
    calls: Mutex<VecDeque<(Req, Rx<Rsp>)>>,
    default: Option<Rsp>,
    expected_workload: Option<String>,
    // hold onto senders for policies that won't be updated so that their
    // streams don't close.
    send_once_txs: Vec<Tx<Rsp>>,
    calls_tx: Option<mpsc::UnboundedSender<Req>>,
}

#[derive(Debug, Clone)]
pub struct InboundSender(Tx<inbound::Server>);

#[derive(Debug, Clone)]
pub struct OutboundSender(Tx<outbound::OutboundPolicy>);

type Tx<T> = mpsc::UnboundedSender<Result<T, grpc::Status>>;
type Rx<T> = UnboundedReceiverStream<Result<T, grpc::Status>>;
type WatchStream<T> = Pin<Box<dyn Stream<Item = Result<T, grpc::Status>> + Send + Sync + 'static>>;

pub fn all_unauthenticated() -> inbound::Server {
    inbound::Server {
        protocol: Some(inbound::ProxyProtocol {
            kind: Some(inbound::proxy_protocol::Kind::Detect(
                inbound::proxy_protocol::Detect {
                    timeout: Some(Duration::from_secs(10).try_into().unwrap()),
                    http_routes: vec![],
                },
            )),
        }),
        authorizations: vec![inbound::Authz {
            networks: vec![inbound::Network {
                net: Some(ipnet::IpNet::default().into()),
                except: Vec::new(),
            }],
            authentication: Some(inbound::Authn {
                permit: Some(inbound::authn::Permit::Unauthenticated(
                    inbound::authn::PermitUnauthenticated {},
                )),
            }),
            labels: Default::default(),
            metadata: Some(api::meta::Metadata {
                kind: Some(api::meta::metadata::Kind::Default(
                    "all-unauthenticated".into(),
                )),
            }),
        }],
        server_ips: vec![],
        labels: maplit::hashmap![
            "name".into() => "all-unauthenticated".into(),
            "kind".into() => "default".into(),
        ],
    }
}

pub fn opaque_unauthenticated() -> inbound::Server {
    inbound::Server {
        protocol: Some(inbound::ProxyProtocol {
            kind: Some(inbound::proxy_protocol::Kind::Opaque(
                inbound::proxy_protocol::Opaque {},
            )),
        }),
        authorizations: vec![inbound::Authz {
            networks: vec![inbound::Network {
                net: Some(ipnet::IpNet::default().into()),
                except: Vec::new(),
            }],
            authentication: Some(inbound::Authn {
                permit: Some(inbound::authn::Permit::Unauthenticated(
                    inbound::authn::PermitUnauthenticated {},
                )),
            }),
            labels: Default::default(),
            metadata: Some(api::meta::Metadata {
                kind: Some(api::meta::metadata::Kind::Default(
                    "all-unauthenticated".into(),
                )),
            }),
        }],
        server_ips: vec![],
        labels: maplit::hashmap![
            "name".into() => "all-unauthenticated".into(),
            "kind".into() => "default".into(),
        ],
    }
}

pub fn outbound_default(dst: impl ToString) -> outbound::OutboundPolicy {
    use outbound::proxy_protocol;
    let dst = dst.to_string();
    let route = outbound_default_http_route(dst.clone());
    outbound::OutboundPolicy {
        protocol: Some(outbound::ProxyProtocol {
            kind: Some(proxy_protocol::Kind::Detect(proxy_protocol::Detect {
                timeout: Some(Duration::from_secs(10).try_into().unwrap()),
                http1: Some(proxy_protocol::Http1 {
                    routes: vec![route.clone()],
                }),
                http2: Some(proxy_protocol::Http2 {
                    routes: vec![route],
                }),
                opaque: Some(proxy_protocol::Opaque {
                    routes: vec![outbound_default_opaque_route(dst)],
                }),
            })),
        }),
    }
}

pub fn outbound_default_http_route(dst: impl ToString) -> outbound::HttpRoute {
    use api::http_route;
    use outbound::distribution;
    outbound::HttpRoute {
        metadata: Some(api::meta::Metadata {
            kind: Some(api::meta::metadata::Kind::Default("default".to_string())),
        }),
        hosts: Vec::new(),
        rules: vec![outbound::http_route::Rule {
            matches: vec![http_route::HttpRouteMatch {
                path: Some(http_route::PathMatch {
                    kind: Some(http_route::path_match::Kind::Prefix("/".to_string())),
                }),
                headers: Vec::new(),
                query_params: Vec::new(),
                method: None,
            }],
            filters: Vec::new(),
            backends: Some(outbound::Distribution {
                distribution: Some(distribution::Distribution::FirstAvailable(
                    distribution::FirstAvailable {
                        backends: vec![backend(dst)],
                    },
                )),
            }),
        }],
    }
}

pub fn outbound_default_http_route(dst: impl ToString) -> outbound::HttpRoute {
    use outbound::distribution;
    outbound::OpaqueRoute {
        metadata: Some(api::meta::Metadata {
            kind: Some(api::meta::metadata::Kind::Default("default".to_string())),
        }),
        rules: vec![outbound::opaque_route::Rule {
            backends: Some(outbound::Distribution {
                distribution: Some(distribution::Distribution::FirstAvailable(
                    distribution::FirstAvailable {
                        backends: vec![backend(dst)],
                    },
                )),
            }),
        }],
    }
}

pub fn backend(dst: impl ToString) -> outbound::Backend {
    use outbound::backend::{self, balance_p2c, endpoint_discovery, EndpointDiscovery};

    outbound::Backend {
        metadata: Some(api::meta::Metadata {
            kind: Some(api::meta::metadata::Kind::Default("default".to_string())),
        }),
        queue: Some(backend::Queue {
            capacity: 100,
            failfast_timeout: Some(Duration::from_secs(3).try_into().unwrap()),
        }),
        kind: Some(backend::Kind::BalanceWeightedDstr(backend::BalanceP2c {
            discovery: Some(EndpointDiscovery {
                kind: Some(endpoint_discovery::Kind::Dst(
                    endpoint_discovery::DestinationGet {
                        path: dst.to_string(),
                    },
                )),
            }),
            load: Some(balance_p2c::Load::PeakEwma(balance_p2c::PeakEwma {
                default_rtt: Some(Duration::from_millis(30).try_into().unwrap()),
                decay: Some(Duration::from_secs(10).try_into().unwrap()),
            })),
        })),
    }
}

impl Controller {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn expect_workload(mut self, workload: String) -> Self {
        self.inbound.expected_workload = Some(workload.clone());
        self.outbound.expected_workload = Some(workload);
        self
    }

    /// Returns an [`InboundSender`] for inbound policies on `port`.
    pub fn inbound_tx(&self, port: u16) -> InboundSender {
        InboundSender(self.inbound.add_call(port))
    }

    /// Sets an inbound policy for `port` that sends a single update and then
    /// remains open.
    pub fn inbound(mut self, port: u16, policy: inbound::Server) -> Self {
        let tx = self.inbound_tx(port);
        tx.send(policy);
        self.inbound.send_once_txs.push(tx.0);
        self
    }

    /// Sets a global default inbound policy.
    pub fn with_inbound_default(mut self, default: inbound::Server) -> Self {
        self.inbound.default = Some(default);
        self
    }

    pub fn inbound_calls(&mut self) -> mpsc::UnboundedReceiver<u16> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.inbound.calls_tx = Some(tx);
        rx
    }

    /// Returns an [`OutboundSender`] for outbound policies for `addr`.
    pub fn outbound_tx(&self, addr: impl Into<Addr>) -> OutboundSender {
        let addr = addr.into();
        let port = addr.port() as u32;
        let target = match addr {
            Addr::Socket(socket) => outbound::traffic_spec::Target::Address(socket.into()),
            Addr::Name(name) => outbound::traffic_spec::Target::Authority(name.to_string()),
        };
        let spec = outbound::TrafficSpec {
            source_workload: String::new(),
            target: Some(target),
        };
        OutboundSender(self.outbound.add_call(spec))
    }

    /// Sets an outbound policy for `addr`` that sends a single update and then
    /// remains open.
    pub fn outbound(mut self, addr: impl Into<Addr>, policy: outbound::OutboundPolicy) -> Self {
        let tx = self.outbound_tx(addr);
        tx.send(policy);
        self.outbound.send_once_txs.push(tx.0);
        self
    }

    /// Sets a default outbound policy for `addr` with destination `dst`, which
    /// sends a single update and then remains open.
    pub fn outbound_default(self, addr: impl Into<Addr>, dst: impl ToString) -> Self {
        self.outbound(addr, outbound_default(dst))
    }

    pub async fn run(self) -> controller::Listening {
        let svc = grpc::transport::Server::builder()
            .add_service(
                inbound_server_policies_server::InboundServerPoliciesServer::new(Server(Arc::new(
                    self.inbound,
                ))),
            )
            .add_service(outbound_policies_server::OutboundPoliciesServer::new(
                Server(Arc::new(self.outbound)),
            ))
            .into_service();
        controller::run(svc, "support policy controller", None).await
    }
}

// === impl InboundSender ===

impl InboundSender {
    pub fn send(&self, up: inbound::Server) {
        self.0.send(Ok(up)).expect("send inbound Server update")
    }

    pub fn send_err(&self, err: grpc::Status) {
        self.0.send(Err(err)).expect("send inbound error")
    }
}

// === impl OutboundSender ===

impl OutboundSender {
    pub fn send(&self, up: outbound::OutboundPolicy) {
        self.0
            .send(Ok(up))
            .expect("send outbound OutboundPolicy update")
    }

    pub fn send_err(&self, err: grpc::Status) {
        self.0.send(Err(err)).expect("send outbound error")
    }
}

// === impl Server ===

#[tonic::async_trait]
impl inbound_server_policies_server::InboundServerPolicies for Server<u16, inbound::Server> {
    type WatchPortStream =
        Pin<Box<dyn Stream<Item = Result<inbound::Server, grpc::Status>> + Send + Sync + 'static>>;

    async fn get_port(
        &self,
        _req: grpc::Request<inbound::PortSpec>,
    ) -> Result<grpc::Response<inbound::Server>, grpc::Status> {
        Err(grpc::Status::new(
            grpc::Code::Unimplemented,
            "the proxy should only make `InboundServerPolicies.WatchPort` RPCs \
            to the inbound policy service, so `GetPort` is not implemented by \
            the test controller",
        ))
    }

    async fn watch_port(
        &self,
        req: grpc::Request<inbound::PortSpec>,
    ) -> Result<grpc::Response<Self::WatchPortStream>, grpc::Status> {
        let req = req.into_inner();
        let _span = tracing::info_span!(
            "InboundPolicies::watch_port",
            req.port,
            %req.workload,
        )
        .entered();
        tracing::debug!(?req, "received request");
        let ret = self.watch_inner(&req.workload, |&spec| req.port as u16 == spec);
        if let Some(ref calls_tx) = self.0.calls_tx {
            let _ = calls_tx.send(req.port as u16);
        }
        ret
    }
}

#[tonic::async_trait]
impl outbound_policies_server::OutboundPolicies
    for Server<outbound::TrafficSpec, outbound::OutboundPolicy>
{
    type WatchStream = Pin<
        Box<
            dyn Stream<Item = Result<outbound::OutboundPolicy, grpc::Status>>
                + Send
                + Sync
                + 'static,
        >,
    >;

    async fn get(
        &self,
        _req: grpc::Request<outbound::TrafficSpec>,
    ) -> Result<grpc::Response<outbound::OutboundPolicy>, grpc::Status> {
        Err(grpc::Status::new(
            grpc::Code::Unimplemented,
            "the proxy should only make `OutboundPolicies.Watch` RPCs to the \
            outbound policy service, so `GetPort` is not implemented by the \
            test controller",
        ))
    }
    async fn watch(
        &self,
        req: grpc::Request<outbound::TrafficSpec>,
    ) -> Result<grpc::Response<Self::WatchStream>, grpc::Status> {
        let req = req.into_inner();
        let _span = tracing::info_span!(
            "OutboundPolicies::watch",
            ?req.target,
            %req.source_workload,
        )
        .entered();
        tracing::debug!(?req, "received request");

        let target = req.target.clone().ok_or_else(|| {
            const ERR: &str = "target is required";
            tracing::warn!(message = %ERR);
            tonic::Status::invalid_argument(ERR)
        })?;
        let ret = self.watch_inner(&req.source_workload, |spec| {
            spec.target.as_ref() == Some(&target)
        });
        if let Some(ref calls_tx) = self.0.calls_tx {
            let _ = calls_tx.send(req);
        }
        ret
    }
}

// === impl Server ===

impl<Req, Rsp> Server<Req, Rsp>
where
    Req: std::fmt::Debug,
    Rsp: std::fmt::Debug + Clone + Send + Sync + 'static,
{
    fn watch_inner(
        &self,
        workload: &str,
        matches: impl Fn(&Req) -> bool,
    ) -> Result<grpc::Response<WatchStream<Rsp>>, grpc::Status> {
        if let Some(ref expected_workload) = self.0.expected_workload {
            if workload != *expected_workload {
                tracing::warn!(
                    actual = ?workload,
                    expected = ?expected_workload,
                    "request workload does not match"
                );
                return Err(grpc_unexpected_request());
            }
        }

        // See if we have any configured expected calls that match this request.
        let mut calls = self.0.calls.lock();
        if let Some((spec, policy)) = calls.pop_front() {
            tracing::debug!(?spec, "checking next call");
            if matches(&spec) {
                tracing::info!(?spec, ?policy, "found request");
                return Ok(grpc::Response::new(Box::pin(policy)));
            }

            tracing::warn!(?spec, ?policy, "request does not match");
            calls.push_front((spec, policy));
        }

        // Try the configured default policy...
        if let Some(default) = self.0.default.clone() {
            tracing::info!("using default policy");
            let stream =
                Box::pin(stream::once(async move { Ok(default) }).chain(stream::pending()));
            return Ok(grpc::Response::new(stream));
        }

        if calls.is_empty() {
            // We're not configured to expect any calls, and we have no default
            // policy. Send a "no results" error.
            Err(grpc_no_results())
        } else {
            // We recieved a request that did not match the expected calls, and
            // we have no default policy. Return an error.
            Err(grpc_unexpected_request())
        }
    }
}

// === impl Inner ===

impl<Req, Rsp> Default for Inner<Req, Rsp> {
    fn default() -> Self {
        Self {
            calls: Mutex::new(VecDeque::new()),
            expected_workload: None,
            default: None,
            send_once_txs: Vec::new(),
            calls_tx: None,
        }
    }
}

impl<Req, Rsp> Inner<Req, Rsp> {
    fn add_call(&self, call: Req) -> mpsc::UnboundedSender<Result<Rsp, grpc::Status>> {
        let (tx, rx) = mpsc::unbounded_channel();
        let rx = UnboundedReceiverStream::new(rx);
        self.calls.lock().push_back((call, rx));
        tx
    }
}

fn grpc_no_results() -> grpc::Status {
    grpc::Status::new(
        grpc::Code::NotFound,
        "unit test policy controller has no results",
    )
}

fn grpc_unexpected_request() -> grpc::Status {
    grpc::Status::new(
        grpc::Code::Unavailable,
        "unit test policy controller expected different request",
    )
}
