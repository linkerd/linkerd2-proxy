use super::*;
use futures::stream;
use parking_lot::Mutex;
use std::{collections::VecDeque, sync::Arc};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tonic as grpc;

pub use linkerd2_proxy_api::{self as api, inbound};

#[derive(Clone, Debug, Default)]
pub struct Controller {
    inbound_calls: Arc<Mutex<VecDeque<(inbound::PortSpec, InboundReceiver)>>>,
    inbound_default: Option<inbound::Server>,
    expected_workload: Option<Arc<String>>,
}

#[derive(Debug, Clone)]
pub struct InboundSender(mpsc::UnboundedSender<Result<inbound::Server, grpc::Status>>);

type InboundReceiver = UnboundedReceiverStream<Result<inbound::Server, grpc::Status>>;

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

    pub fn expect_workload(self, workload: String) -> Self {
        Self {
            expected_workload: Some(workload.into()),
            ..self
        }
    }

    pub fn inbound_tx(&self, port: u16) -> InboundSender {
        let spec = inbound::PortSpec {
            workload: String::new(),
            port: port as u32,
        };

        let (tx, rx) = mpsc::unbounded_channel();
        let rx = UnboundedReceiverStream::new(rx);
        self.inbound_calls.lock().push_back((spec, rx));
        InboundSender(tx)
    }

    pub fn with_inbound_default(mut self, default: inbound::Server) -> Self {
        self.inbound_default = Some(default);
        self
    }

    pub async fn run(self) -> controller::Listening {
        controller::run(
            inbound::inbound_server_policies_server::InboundServerPoliciesServer::new(self),
            "support policy controller",
            None,
        )
        .await
    }
}

impl InboundSender {
    pub fn send(&self, up: inbound::Server) {
        self.0.send(Ok(up)).expect("send inbound Server update")
    }

    pub fn send_err(&self, err: grpc::Status) {
        self.0.send(Err(err)).expect("send inbound error")
    }
}

#[tonic::async_trait]
impl inbound::inbound_server_policies_server::InboundServerPolicies for Controller {
    type WatchPortStream =
        Pin<Box<dyn Stream<Item = Result<inbound::Server, grpc::Status>> + Send + Sync + 'static>>;

    async fn get_port(
        &self,
        _req: grpc::Request<inbound::PortSpec>,
    ) -> Result<grpc::Response<inbound::Server>, grpc::Status> {
        Err(grpc::Status::new(
            grpc::Code::Unimplemented,
            "the proxy should only make `WatchPort` RPCs to the inbound policy \
                service, so `GetPort` is not implemented by the test controller",
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
        tracing::debug!("received request; ");

        if let Some(ref expected_workload) = self.expected_workload {
            if req.workload != **expected_workload {
                tracing::warn!(
                    actual = ?req.workload,
                    expected = ?expected_workload,
                    "request workload does not match"
                );
                return Err(grpc_unexpected_request());
            }
        }

        // See if we have any configured expected calls that match this port.
        let mut calls = self.inbound_calls.lock();
        if let Some((spec, policy)) = calls.pop_front() {
            tracing::debug!(?spec, "checking next call");
            if spec.port == req.port {
                tracing::info!(?spec, ?policy, "found request");
                return Ok(grpc::Response::new(Box::pin(policy)));
            }

            tracing::warn!(?spec, ?policy, "request does not match");
            calls.push_front((spec, policy));
        }

        // Try the configured default policy...
        if let Some(default) = self.inbound_default.clone() {
            tracing::info!("using default inbound policy");
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

fn grpc_no_results() -> grpc::Status {
    grpc::Status::new(
        grpc::Code::Unavailable,
        "unit test policy controller has no results",
    )
}

fn grpc_unexpected_request() -> grpc::Status {
    grpc::Status::new(
        grpc::Code::Unavailable,
        "unit test policy controller expected different request",
    )
}
