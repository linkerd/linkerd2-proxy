#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use futures::future;
use linkerd_error::{Error, Result};
use linkerd_stack::{
    layer::{self, Layer},
    proxy::Proxy,
    util::AndThen,
    Either, NewService, Service,
};
use std::{
    future::Future,
    task::{Context, Poll},
};
pub use tower::retry::{budget::Budget, Policy};
use tracing::trace;

/// A strategy for obtaining per-target retry polices.
pub trait NewPolicy<T> {
    type Policy;

    fn new_policy(&self, target: &T) -> Option<Self::Policy>;
}

/// An extension to [`tower::retry::Policy`] that adds a method to prepare a
/// request to be retried, possibly changing its type.
pub trait PrepareRetry<Req, Rsp>:
    tower::retry::Policy<Self::RetryRequest, Self::RetryResponse, Error>
{
    /// A request type that can be retried.
    ///
    /// This *may* be the same as the `Req` type parameter, but it can also be a
    /// different type, if retries can only be attempted for a specific request type.
    type RetryRequest;

    /// A response type that can be retried.
    ///
    /// This *may* be the same as the `Rsp` type parameter, but it can also be a
    /// different type, if retries can only be attempted for a specific response type.
    type RetryResponse;

    /// The response future.
    ///
    /// If this retry policy doesn't need to asynchronously modify the response
    /// type, this can be `futures::future::Ready`;
    type ResponseFuture: Future<Output = Result<Self::RetryResponse>>;

    /// Prepare an initial request for a potential retry.
    ///
    /// If the request is retryable, this should return `Either::A`. Otherwise,
    /// if this returns `Either::B`, the request will not be retried if it
    /// fails.
    ///
    /// If retrying requires a specific request type other than the input type
    /// to this policy, this function may transform the request into a request
    /// of that type.
    fn prepare_request(self, req: Req) -> Either<(Self, Self::RetryRequest), Req>;

    /// Prepare a response for a potential retry.
    ///
    /// Whether or not the response is retryable is determined by the
    /// [`tower::retry::Policy`] implementation for this type. This method will
    /// be called *prior* to the [`Policy::retry`] method, and provides the
    /// opportunity to (asynchronously) transform the response type prior to
    /// checking if it is retry-able.
    fn prepare_response(rsp: Rsp) -> Self::ResponseFuture;
}

/// Applies per-target retry policies.
#[derive(Clone, Debug)]
pub struct NewRetry<P, N, O> {
    new_policy: P,
    inner: N,
    proxy: O,
}

#[derive(Clone, Debug)]
pub struct Retry<P, S, O> {
    policy: Option<P>,
    inner: S,
    proxy: O,
}

// === impl NewRetry ===

impl<P: Clone, N, O: Clone> NewRetry<P, N, O> {
    pub fn layer(new_policy: P, proxy: O) -> impl Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            new_policy: new_policy.clone(),
            proxy: proxy.clone(),
        })
    }
}

impl<T, N, P, O> NewService<T> for NewRetry<P, N, O>
where
    N: NewService<T>,
    P: NewPolicy<T>,
    O: Clone,
{
    type Service = Retry<P::Policy, N::Service, O>;

    fn new_service(&self, target: T) -> Self::Service {
        // Determine if there is a retry policy for the given target.
        let policy = self.new_policy.new_policy(&target);

        let inner = self.inner.new_service(target);
        Retry {
            policy,
            inner,
            proxy: self.proxy.clone(),
        }
    }
}

// === impl Retry ===

// The inner service created for requests that are retryable.
type RetrySvc<P, S, R, F> = tower::retry::Retry<P, AndThen<S, fn(R) -> F>>;

impl<P, S, O, Req, Fut, Rsp> Service<Req> for Retry<P, S, O>
where
    P: PrepareRetry<Req, Rsp> + Clone + std::fmt::Debug,
    S: Service<Req, Response = Rsp, Future = Fut, Error = Error>,
    S: Service<P::RetryRequest, Response = Rsp, Future = Fut, Error = Error>,
    S: Clone,
    Fut: std::future::Future<Output = Result<Rsp, Error>>,
    O: Proxy<Req, S, Request = Req, Error = Error>,
    O: Proxy<
        P::RetryRequest,
        RetrySvc<P, S, Rsp, P::ResponseFuture>,
        Request = P::RetryRequest,
        Response = <O as Proxy<Req, S>>::Response,
        Error = Error,
    >,
    O: Clone,
{
    type Response = <O as Proxy<Req, S>>::Response;
    type Error = Error;
    type Future = future::Either<
        <O as Proxy<Req, S>>::Future,
        <O as Proxy<P::RetryRequest, RetrySvc<P, S, Rsp, P::ResponseFuture>>>::Future,
    >;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        <S as Service<Req>>::poll_ready(&mut self.inner, cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let (policy, req) = match self.policy.clone() {
            Some(p) => match p.prepare_request(req) {
                Either::A(req) => req,
                Either::B(req) => {
                    return future::Either::Left(self.proxy.proxy(&mut self.inner, req))
                }
            },
            None => return future::Either::Left(self.proxy.proxy(&mut self.inner, req)),
        };
        trace!(retryable = true, ?policy);

        // Take the inner service, replacing it with a clone. This allows the
        // readiness from poll_ready
        let pending = self.inner.clone();
        let ready = std::mem::replace(&mut self.inner, pending);

        // Wrap response bodies (e.g. with WithTrailers) so the retry policy can
        // interact with it.
        let inner = AndThen::new(ready, P::prepare_response as fn(Rsp) -> P::ResponseFuture);

        // Retry::poll_ready is just a pass-through to the inner service, so we
        // can rely on the fact that we've taken the ready inner service handle.
        let mut inner = tower::retry::Retry::new(policy, inner);

        future::Either::Right(self.proxy.proxy(&mut inner, req))
    }
}
