#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

use futures::future;
use linkerd_error::Error;
use linkerd_stack::{layer, Either, NewService, Oneshot, Service, ServiceExt};
use std::task::{Context, Poll};
pub use tower::retry::{budget::Budget, Policy};
use tracing::trace;

/// A strategy for obtaining per-target retry polices.
pub trait NewPolicy<T> {
    type Policy;

    fn new_policy(&self, target: &T) -> Option<Self::Policy>;
}

/// An extension to [`tower::retry::Policy`] that adds a method to prepare a
/// request to be retried, possibly changing its type.
pub trait PrepareRequest<Req, Res, E>: tower::retry::Policy<Self::RetryRequest, Res, E> {
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

/// Applies per-target retry policies.
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

// === impl NewRetry ===

impl<P: Clone, N> NewRetry<P, N> {
    pub fn layer(new_policy: P) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            new_policy: new_policy.clone(),
        })
    }
}

impl<T, N, P> NewService<T> for NewRetry<P, N>
where
    N: NewService<T>,
    P: NewPolicy<T>,
{
    type Service = Retry<P::Policy, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        // Determine if there is a retry policy for the given target.
        let policy = self.new_policy.new_policy(&target);

        let inner = self.inner.new_service(target);
        Retry { policy, inner }
    }
}

// === impl Retry ===

impl<P, Req, S, Rsp, Fut> Service<Req> for Retry<P, S>
where
    P: PrepareRequest<Req, Rsp, Error> + Clone,
    S: Service<Req, Response = Rsp, Future = Fut, Error = Error>
        + Service<P::RetryRequest, Response = Rsp, Future = Fut, Error = Error>
        + Clone,
    Fut: std::future::Future<Output = Result<Rsp, Error>>,
{
    type Response = Rsp;
    type Error = Error;
    type Future = future::Either<Fut, Oneshot<tower::retry::Retry<P, S>, P::RetryRequest>>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        <S as Service<Req>>::poll_ready(&mut self.inner, cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        trace!(retryable = %self.policy.is_some());

        let policy = match self.policy.as_ref() {
            None => return future::Either::Left(self.inner.call(req)),
            Some(p) => p,
        };

        let retry_req = match policy.prepare_request(req) {
            Either::A(retry_req) => retry_req,
            Either::B(req) => return future::Either::Left(self.inner.call(req)),
        };

        let inner = self.inner.clone();
        let retry = tower::retry::Retry::new(policy.clone(), inner);
        future::Either::Right(retry.oneshot(retry_req))
    }
}
