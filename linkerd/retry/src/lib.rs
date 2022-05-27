#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use futures::future;
use linkerd_error::Error;
use linkerd_stack::{
    layer::{self, Layer},
    proxy::{self, Proxy},
    Either, NewService, Service,
};
use std::{
    marker::PhantomData,
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
pub trait PrepareRequest<Req, Rsp, E>: tower::retry::Policy<Self::RetryRequest, Rsp, E> {
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
#[derive(Debug)]
pub struct NewRetry<P, N, R, RReq, O> {
    new_policy: P,
    on_retry: R,
    inner: N,
    on_response: O,
    _r_req: PhantomData<fn(RReq)>,
}

#[derive(Debug)]
pub struct Retry<P, S, R, RReq, O> {
    policy: Option<P>,
    inner: S,
    on_retry: R,
    on_response: O,
    _r_req: PhantomData<fn(RReq)>,
}

// === impl NewRetry ===

impl<P: Clone, N, R: Clone, RReq, O: Clone> NewRetry<P, N, R, RReq, O> {
    pub fn layer(
        new_policy: P,
        on_retry: R,
        on_response: O,
    ) -> impl Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            new_policy: new_policy.clone(),
            on_retry: on_retry.clone(),
            on_response: on_response.clone(),
            _r_req: PhantomData,
        })
    }
}

impl<T, N, P, R, RReq, O> NewService<T> for NewRetry<P, N, R, RReq, O>
where
    N: NewService<T>,
    P: NewPolicy<T>,
    R: Layer<N::Service> + Clone,
    R::Service: Service<RReq>,
    O: Clone,
{
    type Service = Retry<P::Policy, N::Service, R, RReq, O>;

    fn new_service(&self, target: T) -> Self::Service {
        // Determine if there is a retry policy for the given target.
        let policy = self.new_policy.new_policy(&target);

        let inner = self.inner.new_service(target);
        Retry {
            policy,
            inner,
            on_retry: self.on_retry.clone(),
            on_response: self.on_response.clone(),
            _r_req: PhantomData,
        }
    }
}

impl<P: Clone, N: Clone, R: Clone, RReq, O: Clone> Clone for NewRetry<P, N, R, RReq, O> {
    fn clone(&self) -> Self {
        Self {
            new_policy: self.new_policy.clone(),
            on_retry: self.on_retry.clone(),
            inner: self.inner.clone(),
            on_response: self.on_response.clone(),
            _r_req: PhantomData,
        }
    }
}

// === impl Retry ===

impl<P, Req, S, RReq, Fut, R, O> Service<Req> for Retry<P, S, R, RReq, O>
where
    P: PrepareRequest<Req, <R::Service as Service<RReq>>::Response, Error, RetryRequest = RReq>
        + Clone
        + Policy<RReq, <R::Service as Service<RReq>>::Response, Error>,
    S: Service<Req, Future = Fut, Error = Error> + Clone,
    R::Service: Service<RReq, Error = Error> + Clone,
    R: Layer<S>,
    Fut: std::future::Future<Output = Result<S::Response, Error>>,
    O: Proxy<Req, S, Request = Req, Error = Error>,
    O: Proxy<
        RReq,
        tower::retry::Retry<P, R::Service>,
        Request = RReq,
        Response = <O as Proxy<Req, S>>::Response,
        Error = Error,
    >,
    O: Clone,
{
    type Response = <O as Proxy<Req, S>>::Response;
    type Error = Error;
    type Future = future::Either<
        <O as Proxy<Req, S>>::Future,
        proxy::Oneshot<O, tower::retry::Retry<P, R::Service>, RReq>,
    >;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        <S as Service<Req>>::poll_ready(&mut self.inner, cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        trace!(retryable = %self.policy.is_some());

        let policy = match self.policy.as_ref() {
            None => return future::Either::Left(self.on_response.proxy(&mut self.inner, req)),
            Some(p) => p,
        };

        let retry_req = match policy.prepare_request(req) {
            Either::A(retry_req) => retry_req,
            Either::B(req) => {
                return future::Either::Left(self.on_response.proxy(&mut self.inner, req))
            }
        };

        let inner = self.inner.clone();
        let retry = tower::retry::Retry::new(policy.clone(), self.on_retry.layer(inner));
        future::Either::Right(self.on_response.clone().proxy_oneshot(retry, retry_req))
    }
}

impl<P: Clone, S: Clone, R: Clone, RReq, O: Clone> Clone for Retry<P, S, R, RReq, O> {
    fn clone(&self) -> Self {
        Self {
            policy: self.policy.clone(),
            on_retry: self.on_retry.clone(),
            inner: self.inner.clone(),
            on_response: self.on_response.clone(),
            _r_req: PhantomData,
        }
    }
}
