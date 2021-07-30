//! Layer to map service errors into responses.

#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]
use futures::{ready, TryFuture};
use linkerd_error::Error;
use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Creates an error responder for a request.
pub trait NewRespond<Req, Rsp, E = Error> {
    type Response;
    type Respond: Respond<Rsp, E, Response = Self::Response>;

    fn new_respond(&self, req: &Req) -> Self::Respond;
}

/// Creates a response for an error.
pub trait Respond<Rsp, E = Error> {
    type Response;
    fn respond(&self, response: Result<Rsp, E>) -> Result<Self::Response, E>;
}

#[derive(Clone, Debug)]
pub struct RespondLayer<N> {
    new_respond: N,
}

#[derive(Clone, Debug)]
pub struct RespondService<N, S> {
    new_respond: N,
    inner: S,
}

#[pin_project]
#[derive(Debug)]
pub struct RespondFuture<R, F> {
    respond: R,
    #[pin]
    inner: F,
}

impl<N: Clone> RespondLayer<N> {
    pub fn new(new_respond: N) -> Self {
        Self { new_respond }
    }
}

impl<N: Clone, S> tower::layer::Layer<S> for RespondLayer<N> {
    type Service = RespondService<N, S>;

    fn layer(&self, inner: S) -> Self::Service {
        RespondService {
            inner,
            new_respond: self.new_respond.clone(),
        }
    }
}

impl<Req, N, S> tower::Service<Req> for RespondService<N, S>
where
    S: tower::Service<Req>,
    N: NewRespond<Req, S::Response, S::Error>,
{
    type Response = N::Response;
    type Error = S::Error;
    type Future = RespondFuture<N::Respond, S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let respond = self.new_respond.new_respond(&req);
        let inner = self.inner.call(req);
        RespondFuture { respond, inner }
    }
}

impl<R, F> Future for RespondFuture<R, F>
where
    F: TryFuture,
    R: Respond<F::Ok, F::Error>,
{
    type Output = Result<R::Response, F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let rsp = ready!(this.inner.try_poll(cx));
        Poll::Ready(this.respond.respond(rsp))
    }
}
