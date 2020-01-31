#![deny(warnings, rust_2018_idioms)]

use futures::{try_ready, Future, Poll};
use tower::retry;
pub use tower::retry::{budget::Budget, Policy};
use tower::util::{Oneshot, ServiceExt};
use tracing::trace;

pub trait NewPolicy<T> {
    type Policy;

    fn new_policy(&self, target: &T) -> Option<Self::Policy>;
}

#[derive(Clone, Debug)]
pub struct Layer<P> {
    new_policy: P,
}

#[derive(Clone, Debug)]
pub struct MakeRetry<P, N> {
    policy: P,
    inner: N,
}

#[derive(Clone, Debug)]
pub struct Retry<P, S> {
    policy: Option<P>,
    inner: S,
}

pub enum ResponseFuture<P, S, Req>
where
    P: retry::Policy<Req, S::Response, S::Error> + Clone,
    S: tower::Service<Req> + Clone,
{
    Disabled(S::Future),
    Retry(Oneshot<retry::Retry<P, S>, Req>),
}

// === impl Layer ===

impl<P> Layer<P> {
    pub fn new(new_policy: P) -> Self {
        Self { new_policy }
    }
}

impl<P: Clone, N> tower::layer::Layer<N> for Layer<P> {
    type Service = MakeRetry<P, N>;

    fn layer(&self, inner: N) -> Self::Service {
        Self::Service {
            inner,
            policy: self.new_policy.clone(),
        }
    }
}

// === impl MakeRetry ===

impl<T, N, P> tower::Service<T> for MakeRetry<P, N>
where
    N: tower::Service<T>,
    P: NewPolicy<T>,
    P::Policy: Clone,
{
    type Response = Retry<P::Policy, N::Response>;
    type Error = N::Error;
    type Future = MakeRetry<Option<P::Policy>, N::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let policy = self.policy.new_policy(&target);
        let inner = self.inner.call(target);
        MakeRetry { policy, inner }
    }
}

impl<F, P> Future for MakeRetry<Option<P>, F>
where
    F: Future,
    P: Clone,
{
    type Item = Retry<P, F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        let retry = Retry {
            policy: self.policy.clone(),
            inner,
        };
        Ok(retry.into())
    }
}

// === impl Retry ===

impl<P, Req, S> tower::Service<Req> for Retry<P, S>
where
    P: retry::Policy<Req, S::Response, S::Error> + Clone,
    S: tower::Service<Req> + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = ResponseFuture<P, S, Req>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, req: Req) -> Self::Future {
        trace!(retryable = %self.policy.is_some());
        if let Some(policy) = self.policy.clone() {
            let cloned = self.inner.clone();
            let inner = std::mem::replace(&mut self.inner, cloned);
            let retry = retry::Retry::new(policy, inner);
            return ResponseFuture::Retry(retry.oneshot(req));
        }

        ResponseFuture::Disabled(self.inner.call(req))
    }
}

impl<P, S, Req> Future for ResponseFuture<P, S, Req>
where
    P: retry::Policy<Req, S::Response, S::Error> + Clone,
    S: tower::Service<Req> + Clone,
{
    type Item = S::Response;
    type Error = S::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self {
            ResponseFuture::Disabled(ref mut f) => f.poll(),
            ResponseFuture::Retry(ref mut f) => f.poll(),
        }
    }
}
