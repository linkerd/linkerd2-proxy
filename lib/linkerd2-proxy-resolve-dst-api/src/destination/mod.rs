//! A client for the controller's Destination service.
//!
//! This client is split into two primary components: A `Resolver`, that routers use to
//! initiate service discovery for a given name, and a `background::Process` that
//! satisfies these resolution requests. These components are separated by a channel so
//! that the thread responsible for proxying data need not also do this administrative
//! work of communicating with the control plane.
//!
//! The number of active resolutions is not currently bounded by this module. Instead, we
//! trust that callers of `Resolver` enforce such a constraint (for example, via
//! `linkerd2_proxy_router`'s LRU cache). Additionally, users of this module must ensure
//! they consume resolutions as they are sent so that the response channels don't grow
//! without bounds.
//!
//! Furthermore, there are not currently any bounds on the number of endpoints that may be
//! returned for a single resolution. It is expected that the Destination service enforce
//! some reasonable upper bounds.

use crate::api::destination as api;
use crate::metadata::Metadata;
use crate::pb;
use futures::{future, Async, Future, Poll, Stream};
use tower::Service;
use tower_grpc::{self as grpc, generic::client::GrpcService, Body, BoxBody};
use tracing::trace;

pub use crate::core::resolve::Update;

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
            .map(|rsp| Resolution {
                inner: rsp.into_inner(),
            })
    }
}

// === impl ResolveFuture ===

impl<S> Stream for Resolution<S>
where
    S: GrpcService<BoxBody>,
{
    type Item = Update<Metadata>;
    type Error = grpc::Status;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            match self.inner.poll()? {
                Async::NotReady => return Ok(Async::NotReady),

                Async::Ready(Some(api::Update { update })) => match update {
                    Some(api::update::Update::Add(api::WeightedAddrSet {
                        addrs,
                        metric_labels,
                    })) => {
                        let addr_metas = addrs
                            .into_iter()
                            .filter_map(|addr| pb::to_addr_meta(addr, &metric_labels))
                            .collect::<Vec<_>>();
                        if !addr_metas.is_empty() {
                            return Ok(Async::Ready(Some(Update::Add(addr_metas))));
                        }
                    }

                    Some(api::update::Update::Remove(api::AddrSet { addrs })) => {
                        let sock_addrs = addrs
                            .into_iter()
                            .filter_map(pb::to_sock_addr)
                            .collect::<Vec<_>>();
                        if !sock_addrs.is_empty() {
                            return Ok(Async::Ready(Some(Update::Remove(sock_addrs))));
                        }
                    }

                    Some(api::update::Update::NoEndpoints(api::NoEndpoints { exists })) => {
                        let update = if exists {
                            Update::Empty
                        } else {
                            Update::DoesNotExist
                        };
                        return Ok(Async::Ready(Some(update)));
                    }

                    None => {} // continue
                },

                Async::Ready(None) => return Ok(Async::Ready(None)),
            };
        }
    }
}
