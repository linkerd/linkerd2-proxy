use crate::api::destination as api;
use crate::core::resolve::{self, Update};
use crate::metadata::Metadata;
use crate::pb;
use api::destination_client::DestinationClient;
use futures::{ready, Stream};
use http_body::Body as HttpBody;
use pin_project::pin_project;
use std::error::Error;
use std::future::Future;
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

#[pin_project]
pub struct Resolution {
    #[pin]
    inner: grpc::Streaming<api::Update>,
}

// === impl Resolver ===

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
    type Response = Resolution;
    type Error = grpc::Status;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // The future returned by the Tonic generated `DestinationClient`'s `get` method will drive the service to readiness before calling it, so we can always return `Ready` here.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        let path = target.to_string();
        debug!(dst = %path, context = %self.context_token, "Resolving");
        let mut svc = self.service.clone();
        let req = api::GetDestination {
            path,
            scheme: self.scheme.clone(),
            context_token: self.context_token.clone(),
        };
        Box::pin(async move {
            let rsp = svc.get(grpc::Request::new(req)).await?;
            trace!(metadata = ?rsp.metadata());
            Ok(Resolution {
                inner: rsp.into_inner(),
            })
        })
    }
}

impl resolve::Resolution for Resolution {
    type Endpoint = Metadata;
    type Error = grpc::Status;

    fn poll(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Update<Self::Endpoint>, Self::Error>> {
        let mut this = self.project();
        loop {
            match ready!(this.inner.as_mut().poll_next(cx)) {
                Some(update) => match update?.update {
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
                            return Poll::Ready(Ok(Update::Add(addr_metas)));
                        }
                    }

                    Some(api::update::Update::Remove(api::AddrSet { addrs })) => {
                        let sock_addrs = addrs
                            .into_iter()
                            .filter_map(pb::to_sock_addr)
                            .collect::<Vec<_>>();
                        if !sock_addrs.is_empty() {
                            debug!(endpoints = %sock_addrs.len(), "Remove");
                            return Poll::Ready(Ok(Update::Remove(sock_addrs)));
                        }
                    }

                    Some(api::update::Update::NoEndpoints(api::NoEndpoints { exists })) => {
                        info!("No endpoints");
                        let update = if exists {
                            Update::Empty
                        } else {
                            Update::DoesNotExist
                        };
                        return Poll::Ready(Ok(update.into()));
                    }

                    None => {} // continue
                },
                None => {
                    return Poll::Ready(Err(grpc::Status::new(grpc::Code::Ok, "end of stream")))
                }
            };
        }
    }
}
