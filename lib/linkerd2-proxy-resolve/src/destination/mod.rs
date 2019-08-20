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
pub use crate::api::destination::GetDestination as Target;
use crate::metadata::Metadata;
use crate::pb;
use futures::{try_ready, Async, Future, Poll, Stream};
use tower::Service;
use tower_grpc::{self as grpc, generic::client::GrpcService, Body, BoxBody};
use tracing::trace;

pub trait CanResolve {
    fn target(&self) -> Target;
}

/// A handle to request resolutions from the destination service.
#[derive(Clone)]
pub struct Resolve<S>(api::client::Destination<S>);

pub struct ResolveFuture<S: GrpcService<BoxBody>> {
    inner: Option<Inner<S>>,
}

pub struct Resolution<S: GrpcService<BoxBody>> {
    inner: Inner<S>,
}

struct Inner<S: GrpcService<BoxBody>> {
    svc: api::client::Destination<S>,
    target: Target,
    state: State<S>,
}

enum State<S: GrpcService<BoxBody>> {
    NotReady,
    Pending(grpc::client::server_streaming::ResponseFuture<api::Update, S::Future>),
    Streaming(grpc::Streaming<api::Update, S::ResponseBody>),
}

// === impl Resolver ===

impl<S> Resolve<S>
where
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::ResponseBody: Send,
    <S::ResponseBody as Body>::Data: Send,
    S::Future: Send,
{
    /// Returns a `Resolver` for requesting destination resolutions.
    pub fn new(svc: S) -> Self {
        Resolve(api::client::Destination::new(svc))
    }
}

impl<T, S> Service<T> for Resolve<S>
where
    T: CanResolve,
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::ResponseBody: Send,
    <S::ResponseBody as Body>::Data: Send,
    S::Future: Send,
{
    type Response = Resolution<S>;
    type Error = grpc::Status;
    type Future = ResolveFuture<S>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.0.poll_ready()
    }

    /// Start watching for address changes for a certain authority.
    fn call(&mut self, target: T) -> Self::Future {
        let target = target.target();
        trace!("resolve {:?}", target);
        let svc = self.0.clone();
        ResolveFuture {
            inner: Some(Inner {
                svc,
                target,
                state: State::NotReady,
            }),
        }
    }
}

// === impl ResolveFuture ===

impl<S> Future for ResolveFuture<S>
where
    S: GrpcService<BoxBody>,
{
    type Item = Resolution<S>;
    type Error = grpc::Status;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let Inner {
                ref mut state,
                ref mut svc,
                ref target,
            } = self.inner.as_mut().expect("polled after ready");
            *state = match state {
                State::NotReady => {
                    try_ready!(svc.poll_ready());
                    let req = grpc::Request::new(target.clone());
                    State::Pending(svc.get(req))
                }
                State::Pending(ref mut fut) => {
                    let rsp = try_ready!(fut.poll());
                    State::Streaming(rsp.into_inner())
                }
                State::Streaming(_) => break,
            };
        }

        let inner = self.inner.take().expect("polled after ready");
        return Ok(Async::Ready(Resolution { inner }));
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
            self.inner.state = match self.inner.state {
                State::Streaming(ref mut rsp) => match rsp.poll() {
                    Ok(Async::NotReady) => return Ok(Async::NotReady),

                    Ok(Async::Ready(Some(api::Update { update }))) => match update {
                        Some(api::update::Update::Add(api::WeightedAddrSet {
                            addrs,
                            metric_labels,
                            ..
                        })) => {
                            let addrs = addrs
                                .into_iter()
                                .filter_map(|addr| pb::to_addr_meta(addr, &metric_labels))
                                .collect::<Vec<_>>();
                            unimplemented!("add: {}", addrs.len());
                        }

                        Some(api::update::Update::Remove(api::AddrSet {
                            addrs, ..
                        })) => {
                            let addrs = addrs.into_iter().filter_map(pb::to_sock_addr)
                                .collect::<Vec<_>>();
                            unimplemented!("rm: {}", addrs.len());
                        }

                        Some(api::update::Update::NoEndpoints(_)) => {
                            trace!("has no endpoints");
                            unimplemented!("no endpoints")
                        }

                        None => continue,
                    },

                    Ok(Async::Ready(None)) => {
                        return Err(grpc::Status::new(grpc::Code::Ok, "server shutdown"))
                    }

                    Err(e) => {
                        return Err(e);
                    }
                },
            };
        }
    }
}
