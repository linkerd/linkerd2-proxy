#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]
#![allow(clippy::inconsistent_struct_constructor)]
#![allow(clippy::type_complexity)]

use linkerd_error::Error;
use linkerd_stack::{Either, NewService, Proxy, ProxyService};
use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tower::util::{Oneshot, ServiceExt};
use tracing::trace;

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

/// An extension to [`tower::retry::Policy`] that adds a method to prepare a
/// request to be retried, possibly changing its type.
pub trait RetryPolicy<Req, Res, E>: tower::retry::Policy<Self::RetryRequest, Res, E> {
    /// A request type that can be retried.
    ///
    /// This *may* be the same as the `Req` type parameter, but it can also be a
    /// different type, if retries can only be attempted for a specific request type.
    type RetryRequest;

    /// Prepare an initial request for a potential retry.
    ///
    /// If the request is retryable, this should return `Either::A`. Otherwise,
    /// if this returns `Either::B`, the request will not be retried if it
    /// fails.
    ///
    /// If retrying requires a specific request type other than the input type
    /// to this policy, this function may transform the request into a request
    /// of that type.
    fn prepare_request(&self, req: Req) -> Either<Self::RetryRequest, Req>;
}

#[pin_project(project = ResponseFutureProj)]
pub enum ResponseFuture<R, P, S, Req>
where
    R: tower::retry::Policy<Req, P::Response, Error> + Clone,
    P: Proxy<Req, S> + Clone,
    S: tower::Service<P::Request> + Clone,
    S::Error: Into<Error>,
{
    Disabled(#[pin] P::Future),
    Retry(#[pin] Oneshot<tower::retry::Retry<R, ProxyService<P, S>>, Req>),
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

impl<R, P, Req, S, E, PReq, PRsp, PFut> Proxy<Req, S> for Retry<R, P>
where
    R: RetryPolicy<Req, PRsp, Error> + Clone,
    P: Proxy<Req, S, Request = PReq, Response = PRsp, Future = PFut>
        + Proxy<R::RetryRequest, S, Request = PReq, Response = PRsp, Future = PFut>
        + Clone,
    S: tower::Service<PReq, Error = E> + Clone,
    S::Error: Into<Error>,
{
    type Request = PReq;
    type Response = PRsp;
    type Error = Error;
    type Future = ResponseFuture<R, P, S, R::RetryRequest>;

    fn proxy(&self, svc: &mut S, req: Req) -> Self::Future {
        trace!(retryable = %self.policy.is_some());

        if let Some(policy) = self.policy.as_ref() {
            return match policy.prepare_request(req) {
                Either::A(retry_req) => {
                    let inner =
                        Proxy::<R::RetryRequest, S>::wrap_service(self.inner.clone(), svc.clone());
                    let retry = tower::retry::Retry::new(policy.clone(), inner);
                    ResponseFuture::Retry(retry.oneshot(retry_req))
                }
                Either::B(req) => ResponseFuture::Disabled(self.inner.proxy(svc, req)),
            };
        }

        ResponseFuture::Disabled(self.inner.proxy(svc, req))
    }
}

impl<R, P, S, Req> Future for ResponseFuture<R, P, S, Req>
where
    R: tower::retry::Policy<Req, P::Response, Error> + Clone,
    P: Proxy<Req, S> + Clone,
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
