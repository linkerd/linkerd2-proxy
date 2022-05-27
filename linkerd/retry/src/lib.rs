#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use futures::future;
use linkerd_error::Error;
use linkerd_stack::{
    layer::{self, Layer},
    Either, NewService, Oneshot, Service, ServiceExt,
};
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
pub struct NewRetry<P, N, R> {
    new_policy: P,
    on_response: R,
    inner: N,
}

#[derive(Clone, Debug)]
pub struct Retry<P, S, R> {
    policy: Option<P>,
    inner: S,
    on_response: R,
}

// === impl NewRetry ===

impl<P: Clone, N, R: Clone> NewRetry<P, N, R> {
    pub fn layer(new_policy: P, on_response: R) -> impl Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            new_policy: new_policy.clone(),
            on_response: on_response.clone(),
        })
    }
}

impl<T, N, P, R> NewService<T> for NewRetry<P, N, R>
where
    N: NewService<T>,
    P: NewPolicy<T>,
    R: Layer<N::Service> + Clone,
{
    type Service = Retry<P::Policy, N::Service, R>;

    fn new_service(&self, target: T) -> Self::Service {
        // Determine if there is a retry policy for the given target.
        let policy = self.new_policy.new_policy(&target);

        let inner = self.inner.new_service(target);
        Retry {
            policy,
            inner,
            on_response: self.on_response.clone(),
        }
    }
}

// === impl Retry ===

impl<P, Req, S, Rsp, Fut, R> Service<Req> for Retry<P, S, R>
where
    P: PrepareRequest<Req, Rsp, Error> + Clone,
    S: Service<Req, Response = Rsp, Future = Fut, Error = Error> + Clone,
    R::Service: Service<P::RetryRequest, Response = Rsp, Future = Fut, Error = Error> + Clone,
    R: Layer<S>,
    Fut: std::future::Future<Output = Result<Rsp, Error>>,
{
    type Response = Rsp;
    type Error = Error;
    type Future = future::Either<Fut, Oneshot<tower::retry::Retry<P, R::Service>, P::RetryRequest>>;

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
        let retry = tower::retry::Retry::new(policy.clone(), self.on_response.layer(inner));
        future::Either::Right(retry.oneshot(retry_req))
    }
}
