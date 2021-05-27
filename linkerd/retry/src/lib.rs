#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]
#![allow(clippy::inconsistent_struct_constructor)]

use linkerd_error::Error;
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
pub struct NewRetryLayer<P, L = ()> {
    new_policy: P,
    on_retry: L,
}

#[derive(Clone, Debug)]
pub struct NewRetry<P, N, L = ()> {
    new_policy: P,
    inner: N,
    on_retry: L,
}

#[derive(Clone, Debug)]
pub struct Retry<P, S, L = ()> {
    policy: Option<P>,
    inner: S,
    on_retry: L,
}

#[pin_project(project = ResponseFutureProj)]
pub enum ResponseFuture<F, S, Req>
where
    F: futures::TryFuture<Ok = S::Response>,
    F::Error: Into<Error>,
    S: tower::Service<Req> + Clone,
    S::Error: Into<Error>,
{
    Disabled(#[pin] F),
    Retry(#[pin] Oneshot<S, Req>),
}

// === impl NewRetryLayer ===

impl<P> NewRetryLayer<P> {
    pub fn new(new_policy: P) -> Self {
        Self {
            new_policy,
            on_retry: (),
        }
    }

    pub fn on_retry<L: Clone>(self, on_retry: L) -> NewRetryLayer<P, L> {
        NewRetryLayer {
            new_policy: self.new_policy,
            on_retry,
        }
    }
}

impl<P: Clone, N, L: Clone> tower::layer::Layer<N> for NewRetryLayer<P, L> {
    type Service = NewRetry<P, N, L>;

    fn layer(&self, inner: N) -> Self::Service {
        Self::Service {
            inner,
            new_policy: self.new_policy.clone(),
            on_retry: self.on_retry.clone(),
        }
    }
}

// === impl NewRetry ===

impl<T, N, P, L> NewService<T> for NewRetry<P, N, L>
where
    N: NewService<T>,
    P: NewPolicy<T>,
    L: Clone,
{
    type Service = Retry<P::Policy, N::Service, L>;

    fn new_service(&mut self, target: T) -> Self::Service {
        // Determine if there is a retry policy for the given target.
        let policy = self.new_policy.new_policy(&target);

        let inner = self.inner.new_service(target);
        Retry {
            policy,
            inner,
            on_retry: self.on_retry.clone(),
        }
    }
}

// === impl Retry ===

impl<R, P, Req, PReq, S, L> Proxy<Req, S> for Retry<R, P, L>
where
    R: Clone,
    R: tower::retry::Policy<L::Request, L::Response, Error> + Clone,
    P: Proxy<L::Request, S, Response = L::Response, Request = PReq>
        + Proxy<Req, S, Response = L::Response, Request = PReq>
        + Clone,
    S: tower::Service<PReq> + Clone,
    S::Error: Into<Error>,
    L: Proxy<Req, tower::retry::Retry<R, ProxyService<P, S>>> + Clone,
    L::Error: Into<Error>,
{
    type Request = PReq;
    type Response = L::Response;
    type Error = Error;
    type Future = ResponseFuture<
        <P as Proxy<Req, S>>::Future,
        ProxyService<L, tower::retry::Retry<R, ProxyService<P, S>>>,
        Req,
    >;

    fn proxy(&self, svc: &mut S, req: Req) -> Self::Future {
        trace!(retryable = %self.policy.is_some());

        if let Some(policy) = self.policy.as_ref() {
            let inner = <P as Proxy<L::Request, S>>::wrap_service(self.inner.clone(), svc.clone());
            let retry = tower::retry::Retry::new(policy.clone(), inner);
            let retry = self.on_retry.clone().wrap_service(retry);
            return ResponseFuture::Retry(retry.oneshot(req));
        }

        ResponseFuture::Disabled(self.inner.proxy(svc, req))
    }
}

impl<F, S, Req> Future for ResponseFuture<F, S, Req>
where
    F: futures::TryFuture<Ok = S::Response>,
    F::Error: Into<Error>,
    S: tower::Service<Req> + Clone,
    S::Error: Into<Error>,
{
    type Output = Result<F::Ok, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            ResponseFutureProj::Disabled(f) => f.try_poll(cx).map_err(Into::into),
            ResponseFutureProj::Retry(f) => f.poll(cx).map_err(Into::into),
        }
    }
}
