//! Layer to map service errors into responses.

#![deny(
    warnings,
    rust_2018_idioms,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]
#![forbid(unsafe_code)]

use futures::{ready, TryFuture};
use linkerd_error::Error;
use linkerd_stack::{layer, ExtractParam, NewService, Service};
use pin_project::pin_project;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

/// Creates an error responder for a request.
pub trait NewRespond<Req> {
    type Respond;

    fn new_respond(&self, req: &Req) -> Self::Respond;
}

/// Creates a response for an error.
pub trait Respond<Rsp, E = Error> {
    type Response;

    fn respond(&self, response: Result<Rsp, E>) -> Result<Self::Response, E>;
}

#[derive(Clone, Debug)]
pub struct NewRespondService<R, P, N> {
    inner: N,
    params: P,
    _marker: std::marker::PhantomData<fn() -> R>,
}

#[derive(Clone, Debug)]
pub struct RespondService<N, S> {
    inner: S,
    new_respond: N,
}

#[pin_project]
#[derive(Debug)]
pub struct RespondFuture<R, F> {
    respond: R,
    #[pin]
    inner: F,
}

// === impl NewRespondService ===

impl<R, P: Clone, N> NewRespondService<R, P, N> {
    pub fn layer(params: P) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            params: params.clone(),
            _marker: std::marker::PhantomData,
        })
    }
}

impl<T, R, P, N> NewService<T> for NewRespondService<R, P, N>
where
    P: ExtractParam<R, T>,
    N: NewService<T>,
{
    type Service = RespondService<R, N::Service>;

    #[inline]
    fn new_service(&self, target: T) -> Self::Service {
        let new_respond = self.params.extract_param(&target);
        let inner = self.inner.new_service(target);
        RespondService { inner, new_respond }
    }
}

// === impl RespondService ===

impl<Req, R, N, S> Service<Req> for RespondService<R, S>
where
    S: Service<Req>,
    R: NewRespond<Req, Respond = N>,
    N: Respond<S::Response, S::Error>,
{
    type Response = N::Response;
    type Error = S::Error;
    type Future = RespondFuture<N, S::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, req: Req) -> Self::Future {
        let respond = self.new_respond.new_respond(&req);
        let inner = self.inner.call(req);
        RespondFuture { respond, inner }
    }
}

// === impl RespondFuture ===

impl<R, F> Future for RespondFuture<R, F>
where
    F: TryFuture,
    R: Respond<F::Ok, F::Error>,
{
    type Output = Result<R::Response, F::Error>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let rsp = ready!(this.inner.try_poll(cx));
        Poll::Ready(this.respond.respond(rsp))
    }
}
