use crate::api::destination as api;
use crate::core::resolve::{self, Update};
use crate::metadata::Metadata;
use crate::pb;
use api::destination_client::DestinationClient;
use async_stream::try_stream;
use futures::future;
use futures::stream::StreamExt;
use futures::Stream;
use http_body::Body as HttpBody;
use std::error::Error;
use std::pin::Pin;
use std::task::{Context, Poll};
use tonic::{
    self as grpc,
    body::{Body, BoxBody},
    client::GrpcService,
};
use tower::Service;
use tracing::{debug, info, trace};

#[derive(Clone)]
pub struct Resolve<S> {
    service: DestinationClient<S>,
    scheme: String,
    context_token: String,
}

// === impl Resolve ===

impl<S> Resolve<S>
where
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::Error: Into<Box<dyn Error + Send + Sync + 'static>> + Send,
    S::ResponseBody: Send,
    <S::ResponseBody as Body>::Data: Send,
    <S::ResponseBody as HttpBody>::Error: Into<Box<dyn Error + Send + Sync + 'static>> + Send,
    S::Future: Send,
{
    pub fn new(svc: S) -> Self {
        Self {
            service: DestinationClient::new(svc),
            scheme: "".into(),
            context_token: "".into(),
        }
    }

    pub fn with_scheme<T: ToString>(self, scheme: T) -> Self {
        Self {
            scheme: scheme.to_string(),
            ..self
        }
    }

    pub fn with_context_token<T: ToString>(self, context_token: T) -> Self {
        Self {
            context_token: context_token.to_string(),
            ..self
        }
    }
}

type UpdatesStream =
    Pin<Box<dyn Stream<Item = Result<Update<Metadata>, grpc::Status>> + Send + 'static>>;

impl<T, S> Service<T> for Resolve<S>
where
    T: ToString,
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::Error: Into<Box<dyn Error + Send + Sync>> + Send,
    S::ResponseBody: Send,
    <S::ResponseBody as Body>::Data: Send,
    <S::ResponseBody as HttpBody>::Error: Into<Box<dyn Error + Send + Sync + 'static>> + Send,
    S::Future: Send,
{
    type Response = UpdatesStream;
    type Error = grpc::Status;
    type Future = futures::future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // The future returned by the Tonic generated `DestinationClient`'s `get` method will drive the service to readiness before calling it, so we can always return `Ready` here.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        let path = target.to_string();
        debug!(dst = %path, context = %self.context_token, "Resolving");
        let req = api::GetDestination {
            path,
            scheme: self.scheme.clone(),
            context_token: self.context_token.clone(),
        };

        future::ok(Box::pin(resolution(self.service.clone(), req)))
    }
}

fn resolution<S>(
    mut client: DestinationClient<S>,
    req: api::GetDestination,
) -> impl Stream<Item = Result<resolve::Update<Metadata>, grpc::Status>>
where
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::Error: Into<Box<dyn Error + Send + Sync>> + Send,
    S::ResponseBody: Send,
    <S::ResponseBody as Body>::Data: Send,
    <S::ResponseBody as HttpBody>::Error: Into<Box<dyn Error + Send + Sync + 'static>> + Send,
    S::Future: Send,
{
    try_stream! {
            let rsp = client.get(grpc::Request::new(req)).await?;
            trace!(metadata = ?rsp.metadata());
            let mut stream = rsp.into_inner();
    loop {
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
                            Update::Empty
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
}
