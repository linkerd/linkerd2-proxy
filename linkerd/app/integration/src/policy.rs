use super::*;
use parking_lot::Mutex;
use std::{collections::VecDeque, net::SocketAddr, sync::Arc};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tonic as grpc;

pub use linkerd2_proxy_api::{inbound, outbound};

#[derive(Clone, Debug, Default)]
pub struct Controller {
    outbound_calls: Arc<Mutex<VecDeque<(outbound::TargetSpec, OutboundReceiver)>>>,
    inbound_calls: Arc<Mutex<VecDeque<(inbound::PortSpec, InboundReceiver)>>>,
    expected_workload: Option<Arc<String>>,
}

#[derive(Debug, Clone)]
pub struct OutboundSender(mpsc::UnboundedSender<Result<outbound::Service, grpc::Status>>);

type OutboundReceiver = UnboundedReceiverStream<Result<outbound::Service, grpc::Status>>;

#[derive(Debug, Clone)]
struct OutboundSvc {
    expected_calls: Arc<Mutex<VecDeque<(outbound::TargetSpec, OutboundReceiver)>>>,
    expected_workload: Option<Arc<String>>,
}

#[derive(Debug, Clone)]
pub struct InboundSender(mpsc::UnboundedSender<Result<inbound::Server, grpc::Status>>);

type InboundReceiver = UnboundedReceiverStream<Result<inbound::Server, grpc::Status>>;

#[derive(Debug, Clone)]
struct InboundSvc {
    expected_calls: Arc<Mutex<VecDeque<(inbound::PortSpec, InboundReceiver)>>>,
    expected_workload: Option<Arc<String>>,
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

    pub fn outbound_tx(&self, addr: impl Into<SocketAddr>) -> OutboundSender {
        let addr = addr.into();
        let spec = outbound::TargetSpec {
            workload: String::new(),
            address: Some(addr.ip().into()),
            port: addr.port() as u32,
        };

        let (tx, rx) = mpsc::unbounded_channel();
        let rx = UnboundedReceiverStream::new(rx);
        self.outbound_calls.lock().push_back((spec, rx));
        OutboundSender(tx)
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

    pub async fn run(self) -> controller::Listening {
        let svc = grpc::transport::Server::builder()
            .add_service(
                inbound::inbound_server_policies_server::InboundServerPoliciesServer::new(
                    InboundSvc {
                        expected_calls: self.inbound_calls,
                        expected_workload: self.expected_workload.clone(),
                    },
                ),
            )
            .add_service(
                outbound::outbound_policies_server::OutboundPoliciesServer::new(OutboundSvc {
                    expected_calls: self.outbound_calls,
                    expected_workload: self.expected_workload,
                }),
            )
            .into_service();
        controller::run(svc, "support policy controller", None).await
    }
}

#[tonic::async_trait]
impl outbound::outbound_policies_server::OutboundPolicies for OutboundSvc {
    type WatchStream = OutboundReceiver;

    async fn get(
        &self,
        _req: grpc::Request<outbound::TargetSpec>,
    ) -> Result<grpc::Response<outbound::Service>, grpc::Status> {
        Err(grpc::Status::new(
            grpc::Code::Unimplemented,
            "the proxy should only make `Watch` RPCs to the outbound policy \
                service, so `Get` is not implemented by the test controller",
        ))
    }
    async fn watch(
        &self,
        req: grpc::Request<outbound::TargetSpec>,
    ) -> Result<grpc::Response<Self::WatchStream>, grpc::Status> {
        let req = req.into_inner();
        let _span = tracing::info_span!(
            "OutboundPolicies::watch",
            ?req.address,
            req.port,
            %req.workload,
        )
        .entered();
        tracing::debug!("received request");

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

        let mut calls = self.expected_calls.lock();
        if let Some((spec, policy)) = calls.pop_front() {
            tracing::debug!(?spec, "checking next call");

            if spec.address == req.address && spec.port == req.port {
                tracing::info!(?spec, ?policy, "found request");
                return Ok(grpc::Response::new(policy));
            }

            tracing::warn!(?spec, ?policy, "request does not match");
            calls.push_front((spec, policy));
            return Err(grpc_unexpected_request());
        }

        Err(grpc_no_results())
    }
}

#[tonic::async_trait]
impl inbound::inbound_server_policies_server::InboundServerPolicies for InboundSvc {
    type WatchPortStream = InboundReceiver;

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
        tracing::debug!("received request");

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

        let mut calls = self.expected_calls.lock();
        if let Some((spec, policy)) = calls.pop_front() {
            tracing::debug!(?spec, "checking next call");
            if spec.port == req.port {
                tracing::info!(?spec, ?policy, "found request");
                return Ok(grpc::Response::new(policy));
            }

            tracing::warn!(?spec, ?policy, "request does not match");
            calls.push_front((spec, policy));
            return Err(grpc_unexpected_request());
        }

        Err(grpc_no_results())
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
