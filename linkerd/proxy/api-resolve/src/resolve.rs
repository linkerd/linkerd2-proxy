use crate::{api::destination as api, core::resolve::Update, metadata::Metadata, pb, ConcreteAddr};
use api::destination_client::DestinationClient;
use futures::prelude::*;
use http_body::Body;
use linkerd_error::Error;
use linkerd_stack::Param;
use linkerd_tonic_stream::{LimitReceiveFuture, ReceiveLimits};
use std::pin::Pin;
use std::task::{Context, Poll};
use tonic::{self as grpc, body::BoxBody, client::GrpcService};
use tower::Service;
use tracing::{debug, info, trace};

#[derive(Clone)]
pub struct Resolve<S> {
    client: DestinationClient<S>,
    context_token: String,
    limits: ReceiveLimits,
}

// === impl Resolve ===

impl<S> Resolve<S>
where
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::Error: Into<Error> + Send,
    S::ResponseBody: Body<Data = tonic::codegen::Bytes> + Send + 'static,
    <S::ResponseBody as Body>::Error: Into<Error> + Send,
    S::Future: Send,
{
    pub fn new(svc: S, context_token: String, limits: ReceiveLimits) -> Self {
        Self {
            client: DestinationClient::new(svc),
            context_token,
            limits,
        }
    }
}

type UpdatesStream =
    Pin<Box<dyn Stream<Item = Result<Update<Metadata>, grpc::Status>> + Send + 'static>>;

type ResolveFuture =
    Pin<Box<dyn Future<Output = Result<UpdatesStream, grpc::Status>> + Send + 'static>>;

impl<T, S> Service<T> for Resolve<S>
where
    T: Param<ConcreteAddr>,
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::Error: Into<Error> + Send,
    S::ResponseBody: Body<Data = tonic::codegen::Bytes> + Send + 'static,
    <S::ResponseBody as Body>::Error: Into<Error> + Send,
    S::Future: Send,
{
    type Response = UpdatesStream;
    type Error = grpc::Status;
    type Future = ResolveFuture;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        let ConcreteAddr(addr) = target.param();
        debug!(dst = %addr, context = %self.context_token, "Resolving");

        let req = api::GetDestination {
            path: addr.to_string(),
            context_token: self.context_token.clone(),
            ..Default::default()
        };

        let limits = self.limits;
        let mut client = self.client.clone();
        Box::pin(async move {
            let rsp = LimitReceiveFuture::new(limits, client.get(grpc::Request::new(req))).await?;
            trace!(metadata = ?rsp.metadata());
            Ok(rsp
                .into_inner()
                .try_filter_map(|up| futures::future::ok::<_, _>(mk_update(up)))
                .boxed())
        })
    }
}

fn mk_update(up: api::Update) -> Option<Update<Metadata>> {
    match up.update? {
        api::update::Update::Add(api::WeightedAddrSet {
            addrs,
            metric_labels,
        }) => {
            let addr_metas = addrs
                .into_iter()
                .filter_map(|addr| pb::to_addr_meta(addr, &metric_labels))
                .collect::<Vec<_>>();
            if !addr_metas.is_empty() {
                debug!(endpoints = %addr_metas.len(), "Add");
                return Some(Update::Add(addr_metas));
            }
        }

        api::update::Update::Remove(api::AddrSet { addrs }) => {
            let sock_addrs = addrs
                .into_iter()
                .filter_map(pb::to_sock_addr)
                .collect::<Vec<_>>();
            if !sock_addrs.is_empty() {
                debug!(endpoints = %sock_addrs.len(), "Remove");
                return Some(Update::Remove(sock_addrs));
            }
        }

        api::update::Update::NoEndpoints(api::NoEndpoints { exists }) => {
            info!("No endpoints");
            return Some(if exists {
                Update::Reset(Vec::new())
            } else {
                Update::DoesNotExist
            });
        }
    }

    None
}
