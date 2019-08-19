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

pub use crate::api::destination::GetDestination as Target;
use crate::api::destination::{client::Destination, Update as ApiUpdate};
use crate::core::resolve;
use crate::metadata::Metadata;
use futures::{try_ready, Async, Future, Poll};
use tower_grpc::{self as grpc, generic::client::GrpcService, Body, BoxBody};
use tracing::trace;

pub trait CanResolve {
    fn target(&self) -> Target;
}

/// A handle to request resolutions from the destination service.
#[derive(Clone)]
pub struct Resolve<S>(Destination<S>);

pub struct ResolveFuture<S: GrpcService<BoxBody>> {
    inner: Option<Inner<S>>,
}

pub struct Resolution<S: GrpcService<BoxBody>> {
    inner: Inner<S>,
}

struct Inner<S: GrpcService<BoxBody>> {
    svc: Destination<S>,
    target: Target,
    state: State<S>,
}

enum State<S: GrpcService<BoxBody>> {
    NotReady,
    Pending(grpc::client::server_streaming::ResponseFuture<ApiUpdate, S::Future>),
    Streaming(grpc::Streaming<ApiUpdate, S::ResponseBody>),
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
    pub fn new(svc: T) -> Self {
        Resolve(Destination::new(svc))
    }
}

impl<T, S> resolve::Resolve<T> for Resolve<S>
where
    T: CanResolve,
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::ResponseBody: Send,
    <S::ResponseBody as Body>::Data: Send,
    S::Future: Send,
{
    type Endpoint = Metadata;
    type Future = ResolveFuture<S>;
    type Resolution = Resolution<S>;

    /// Start watching for address changes for a certain authority.
    fn resolve(&self, target: &T) -> Self::Future {
        let target = target.target();
        trace!("resolve {:?}", dst);
        let svc = self.0.clone();
        ResolveFuture {
            svc,
            target,
            state: State::NotReady,
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
            let Inner { ref mut state, ref mut svc, ref target } = self.inner.as_mut().expect("polled after ready");
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

impl<S> resolve::Resolution for Resolution<S>
where
    S: GrpcService<BoxBody>,
{
    type Endpoint = Metadata;

    loop {
        self.inner.state = match self.inner.state {
            State::Streaming(ref mut rsp) => {
                match rsp.poll() {
                    Ok(Async::NotReady) => Ok(Async::NotReady),
                    Ok(Async::Ready(Some(update))) => {
                        unimplemented!();
                    }
                    Ok(Async::Ready(None)) => {
                        unimplemented!();
                    }
                }
            }
        };
    }
}
