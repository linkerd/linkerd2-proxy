use super::*;
pub use api::inbound;
use api::inbound::inbound_server_policies_server;
use futures::stream;
use linkerd2_proxy_api as api;
use parking_lot::Mutex;
use std::{collections::VecDeque, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tonic as grpc;

#[derive(Debug, Default)]
pub struct Controller {
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

impl Controller {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn expect_workload(mut self, workload: String) -> Self {
        self.inbound.expected_workload = Some(workload.clone());
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

    pub async fn run(self) -> controller::Listening {
        let svc = grpc::transport::Server::builder()
            .add_service(
                inbound_server_policies_server::InboundServerPoliciesServer::new(Server(Arc::new(
                    self.inbound,
                ))),
            )
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
