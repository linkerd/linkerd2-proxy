#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]
#![allow(clippy::inconsistent_struct_constructor)]
#![allow(clippy::type_complexity)]

use linkerd_error::Error;
use linkerd_http_box::BoxBody;
use linkerd_stack::{NewService, Proxy, ProxyService};
use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
pub use tower::retry::{budget::Budget, Policy};
use tower::util::{Oneshot, ServiceExt};
use tracing::trace;

pub mod replay;
pub use self::replay::ReplayBody;

/// A strategy for obtaining per-target retry polices.
pub trait NewPolicy<T> {
    type Policy;

    fn new_policy(&self, target: &T) -> Option<Self::Policy>;
}

/// A layer that applies per-target retry polcies.
///
/// Composes `NewService`s that produce a `Proxy`.
#[derive(Clone, Debug)]
pub struct NewRetryLayer<P> {
    new_policy: P,
}

#[derive(Clone, Debug)]
pub struct NewRetry<P, N> {
    new_policy: P,
    inner: N,
}

#[derive(Clone, Debug)]
pub struct Retry<P, S> {
    policy: Option<P>,
    inner: S,
}

pub trait CanRetry<B> {
    /// Returns `true` if a request can be retried.
    fn can_retry(&self, req: &http::Request<B>) -> bool;
}

#[pin_project(project = ResponseFutureProj)]
pub enum ResponseFuture<R, P, S, B>
where
    R: tower::retry::Policy<http::Request<ReplayBody<B>>, P::Response, Error> + Clone,
    P: Proxy<http::Request<BoxBody>, S> + Clone,
    S: tower::Service<P::Request> + Clone,
    S::Error: Into<Error>,
{
    Disabled(#[pin] P::Future),
    Retry(
        #[pin]
        Oneshot<
            tower::retry::Retry<
                R,
                tower::util::MapRequest<
                    ProxyService<P, S>,
                    fn(http::Request<ReplayBody<B>>) -> http::Request<BoxBody>,
                >,
            >,
            http::Request<ReplayBody<B>>,
        >,
    ),
}

// === impl NewRetryLayer ===

impl<P> NewRetryLayer<P> {
    pub fn new(new_policy: P) -> Self {
        Self { new_policy }
    }
}

impl<P: Clone, N> tower::layer::Layer<N> for NewRetryLayer<P> {
    type Service = NewRetry<P, N>;

    fn layer(&self, inner: N) -> Self::Service {
        Self::Service {
            inner,
            new_policy: self.new_policy.clone(),
        }
    }
}

// === impl NewRetry ===

impl<T, N, P> NewService<T> for NewRetry<P, N>
where
    N: NewService<T>,
    P: NewPolicy<T>,
{
    type Service = Retry<P::Policy, N::Service>;

    fn new_service(&mut self, target: T) -> Self::Service {
        // Determine if there is a retry policy for the given target.
        let policy = self.new_policy.new_policy(&target);

        let inner = self.inner.new_service(target);
        Retry { policy, inner }
    }
}

// === impl Retry ===

impl<R, P, B, S> Proxy<http::Request<B>, S> for Retry<R, P>
where
    R: tower::retry::Policy<http::Request<ReplayBody<B>>, P::Response, Error> + CanRetry<B> + Clone,
    P: Proxy<http::Request<BoxBody>, S> + Clone,
    S: tower::Service<P::Request> + Clone,
    S::Error: Into<Error>,
    B: http_body::Body + Unpin + Send + 'static,
    B::Data: Send,
    B::Error: Into<Error>,
{
    type Request = P::Request;
    type Response = P::Response;
    type Error = Error;
    type Future = ResponseFuture<R, P, S, B>;

    fn proxy(&self, svc: &mut S, req: http::Request<B>) -> Self::Future {
        trace!(retryable = %self.policy.is_some());

        if let Some(policy) = self.policy.as_ref() {
            if policy.can_retry(&req) {
                let inner = self.inner.clone().wrap_service(svc.clone()).map_request(
                    (|req: http::Request<ReplayBody<B>>| req.map(BoxBody::new)) as fn(_) -> _,
                );
                let retry = tower::retry::Retry::new(policy.clone(), inner);
                return ResponseFuture::Retry(
                    retry.oneshot(req.map(|x| ReplayBody::new(x, 64 * 1024))),
                );
            }
        }

        ResponseFuture::Disabled(self.inner.proxy(svc, req.map(BoxBody::new)))
    }
}

impl<R, P, S, B> Future for ResponseFuture<R, P, S, B>
where
    R: tower::retry::Policy<http::Request<ReplayBody<B>>, P::Response, Error> + Clone,
    P: Proxy<http::Request<BoxBody>, S> + Clone,
    S: tower::Service<P::Request> + Clone,
    S::Error: Into<Error>,
{
    type Output = Result<P::Response, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            ResponseFutureProj::Disabled(f) => f.poll(cx).map_err(Into::into),
            ResponseFutureProj::Retry(f) => f.poll(cx).map_err(Into::into),
        }
    }
}
