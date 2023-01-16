use crate::{
    api::destination as api,
    core::resolve::{self, Update},
    metadata::Metadata,
    pb, DestinationGetPath,
};
use api::destination_client::DestinationClient;
use async_stream::try_stream;
use futures::prelude::*;
use http_body::Body;
use linkerd_error::Error;
use linkerd_stack::Param;
use std::pin::Pin;
use std::task::{Context, Poll};
use tonic::{self as grpc, body::BoxBody, client::GrpcService};
use tower::Service;
use tracing::{debug, info, trace};

#[derive(Clone)]
pub struct Resolve<S> {
    service: DestinationClient<S>,
    context_token: String,
}

// === impl Resolve ===

impl<S> Resolve<S>
where
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::Error: Into<Error> + Send,
    S::ResponseBody: Default + Body<Data = tonic::codegen::Bytes> + Send + 'static,
    <S::ResponseBody as Body>::Error: Into<Error> + Send,
    S::Future: Send,
{
    pub fn new(svc: S, context_token: String) -> Self {
        Self {
            service: DestinationClient::new(svc),
            context_token,
        }
    }
}

type UpdatesStream =
    Pin<Box<dyn Stream<Item = Result<Update<Metadata>, grpc::Status>> + Send + 'static>>;

type ResolveFuture =
    Pin<Box<dyn Future<Output = Result<UpdatesStream, grpc::Status>> + Send + 'static>>;

impl<T, S> Service<T> for Resolve<S>
where
    T: Param<DestinationGetPath>,
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::Error: Into<Error> + Send,
    S::ResponseBody: Default + Body<Data = tonic::codegen::Bytes> + Send + 'static,
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
        let DestinationGetPath(path) = target.param();
        debug!(%path, context = %self.context_token, "Resolving");

        let req = api::GetDestination {
            path,
            context_token: self.context_token.clone(),
            ..Default::default()
        };
        let mut client = self.service.clone();
        Box::pin(async move {
            // Wait for the server to respond once before returning a stream. This let's us eagerly
            // detect errors (like InvalidArgument).
            let rsp = client.get(grpc::Request::new(req)).await?;
            trace!(metadata = ?rsp.metadata());
            let stream: UpdatesStream = Box::pin(resolution(rsp.into_inner()));
            Ok(stream)
        })
    }
}

fn resolution(
    mut stream: tonic::Streaming<api::Update>,
) -> impl Stream<Item = Result<resolve::Update<Metadata>, grpc::Status>> {
    try_stream! {
        while let Some(update) = stream.next().await {
            match update?.update {
                Some(api::update::Update::Add(api::WeightedAddrSet {
                    addrs,
                    metric_labels,
                })) => {
                    let addr_metas = addrs
                        .into_iter()
                        .filter_map(|addr| pb::to_addr_meta(addr, &metric_labels))
                        .collect::<Vec<_>>();
                    if !addr_metas.is_empty() {
                        debug!(endpoints = %addr_metas.len(), "Add");
                        yield Update::Add(addr_metas);
                    }
                }

                Some(api::update::Update::Remove(api::AddrSet { addrs })) => {
                    let sock_addrs = addrs
                        .into_iter()
                        .filter_map(pb::to_sock_addr)
                        .collect::<Vec<_>>();
                    if !sock_addrs.is_empty() {
                        debug!(endpoints = %sock_addrs.len(), "Remove");
                        yield Update::Remove(sock_addrs);
                    }
                }

                Some(api::update::Update::NoEndpoints(api::NoEndpoints { exists })) => {
                    info!("No endpoints");
                    let update = if exists {
                        Update::Reset(Vec::new())
                    } else {
                        Update::DoesNotExist
                    };
                    yield update.into();
                }

                None => {} // continue
            }
        }
    }
}
