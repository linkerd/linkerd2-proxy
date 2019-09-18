use crate::api::destination as api;
use crate::core::resolve::{self, Update};
use crate::metadata::Metadata;
use crate::pb;
use futures::{future, try_ready, Future, Poll, Stream};
use tower::Service;
use tower_grpc::{self as grpc, generic::client::GrpcService, Body, BoxBody};
use tracing::{debug, trace};

#[derive(Clone)]
pub struct Resolve<S> {
    service: api::client::Destination<S>,
    scheme: String,
    context_token: String,
}

pub struct Resolution<S: GrpcService<BoxBody>> {
    inner: grpc::Streaming<api::Update, S::ResponseBody>,
}

// === impl Resolver ===

impl<S> Resolve<S>
where
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::ResponseBody: Send,
    <S::ResponseBody as Body>::Data: Send,
    S::Future: Send,
{
    pub fn new(svc: S) -> Self {
        Self {
            service: api::client::Destination::new(svc),
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
    S::ResponseBody: Send,
    <S::ResponseBody as Body>::Data: Send,
    S::Future: Send,
{
    type Response = Resolution<S>;
    type Error = grpc::Status;
    type Future = future::Map<
        grpc::client::server_streaming::ResponseFuture<api::Update, S::Future>,
        fn(grpc::Response<grpc::Streaming<api::Update, S::ResponseBody>>) -> Resolution<S>,
    >;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let path = target.to_string();
        trace!("resolve {:?}", path);
        self.service
            .get(grpc::Request::new(api::GetDestination {
                path,
                scheme: self.scheme.clone(),
                context_token: self.context_token.clone(),
            }))
            .map(|rsp| {
                debug!(metadata = ?rsp.metadata());
                Resolution {
                    inner: rsp.into_inner(),
                }
            })
    }
}

// === impl ResolveFuture ===

impl<S> resolve::Resolution for Resolution<S>
where
    S: GrpcService<BoxBody>,
{
    type Endpoint = Metadata;
    type Error = grpc::Status;

    fn poll(&mut self) -> Poll<Update<Self::Endpoint>, Self::Error> {
        loop {
            match try_ready!(self.inner.poll()) {
                Some(api::Update { update }) => match update {
                    Some(api::update::Update::Add(api::WeightedAddrSet {
                        addrs,
                        metric_labels,
                    })) => {
                        let addr_metas = addrs
                            .into_iter()
                            .filter_map(|addr| pb::to_addr_meta(addr, &metric_labels))
                            .collect::<Vec<_>>();
                        if !addr_metas.is_empty() {
                            return Ok(Update::Add(addr_metas).into());
                        }
                    }

                    Some(api::update::Update::Remove(api::AddrSet { addrs })) => {
                        let sock_addrs = addrs
                            .into_iter()
                            .filter_map(pb::to_sock_addr)
                            .collect::<Vec<_>>();
                        if !sock_addrs.is_empty() {
                            return Ok(Update::Remove(sock_addrs).into());
                        }
                    }

                    Some(api::update::Update::NoEndpoints(api::NoEndpoints { exists })) => {
                        let update = if exists {
                            Update::Empty
                        } else {
                            Update::DoesNotExist
                        };
                        return Ok(update.into());
                    }

                    None => {} // continue
                },

                None => return Err(grpc::Status::new(grpc::Code::Ok, "end of stream")),
            };
        }
    }
}
