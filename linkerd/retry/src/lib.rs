#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use futures::future;
use linkerd_error::Error;
use linkerd_stack::{
    layer::{self, Layer},
    proxy::{Proxy, ProxyService},
    util::AndThen,
    Either, NewService, Oneshot, Service, ServiceExt,
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
pub trait PrepareRetry<Req, Rsp, E>:
    tower::retry::Policy<Self::RetryRequest, Self::RetryResponse, E>
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
    type ResponseFuture: Future<Output = Result<Self::RetryResponse, E>>;

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

#[derive(Clone, Debug)]
pub struct NewRetryLayer<P, O = ()> {
    new_policy: P,
    proxy: O,
}

// === impl NewRetryLayer ===
pub fn layer<P>(new_policy: P) -> NewRetryLayer<P> {
    NewRetryLayer {
        new_policy,
        proxy: (),
    }
}

impl<P, O, N> Layer<N> for NewRetryLayer<P, O>
where
    P: Clone,
    O: Clone,
{
    type Service = NewRetry<P, N, O>;
    fn layer(&self, inner: N) -> Self::Service {
        NewRetry {
            inner,
            new_policy: self.new_policy.clone(),
            proxy: self.proxy.clone(),
        }
    }
}

impl<P> NewRetryLayer<P, ()> {
    /// Adds a [`Proxy`] that will be applied to both the inner service and the
    /// retry service.
    ///
    /// By default, this is the identity proxy, and does nothing.
    pub fn with_proxy<O>(self, proxy: O) -> NewRetryLayer<P, O> {
        NewRetryLayer {
            new_policy: self.new_policy,
            proxy,
        }
    }
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

impl<P, S, O, Req, Fut, Rsp> Service<Req> for Retry<P, S, O>
where
    P: PrepareRetry<Req, Rsp, Error> + Clone,
    S: Service<Req, Response = Rsp, Future = Fut, Error = Error>,
    S: Service<P::RetryRequest, Response = Rsp, Future = Fut, Error = Error>,
    S: Clone,
    Fut: std::future::Future<Output = Result<Rsp, Error>>,
    O: Proxy<Req, S, Request = Req, Error = Error>,
    O: Proxy<
        P::RetryRequest,
        tower::retry::Retry<
            P,
            AndThen<S, fn(<S as Service<P::RetryRequest>>::Response) -> P::ResponseFuture>,
        >,
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
        Oneshot<
            ProxyService<
                O,
                tower::retry::Retry<
                    P,
                    AndThen<S, fn(<S as Service<P::RetryRequest>>::Response) -> P::ResponseFuture>,
                >,
            >,
            P::RetryRequest,
        >,
    >;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        <S as Service<Req>>::poll_ready(&mut self.inner, cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        trace!(retryable = %self.policy.is_some());

        let policy = match self.policy.as_ref() {
            None => return future::Either::Left(self.proxy.proxy(&mut self.inner, req)),
            Some(p) => p,
        };

        let retry_req = match policy.prepare_request(req) {
            Either::A(retry_req) => retry_req,
            Either::B(req) => return future::Either::Left(self.proxy.proxy(&mut self.inner, req)),
        };

        let inner = AndThen::new(
            self.inner.clone(),
            P::prepare_response as fn(Rsp) -> P::ResponseFuture,
        );
        let retry = tower::retry::Retry::new(policy.clone(), inner);
        let retry = self.proxy.clone().into_service(retry);
        future::Either::Right(retry.oneshot(retry_req))
    }
}
