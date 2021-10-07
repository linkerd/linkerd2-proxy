use futures::TryFutureExt;
use linkerd_error::Error;
use std::task::{Context, Poll};

/// Like `tower::util::MapErr`, but with an implementation of `Proxy`.
#[derive(Clone, Debug)]
pub struct MapErr<S, F> {
    inner: S,
    f: F,
}

impl<S, F: Clone> MapErr<S, F> {
    pub fn new(inner: S, f: F) -> Self {
        Self { inner, f }
    }

    pub fn layer(f: F) -> impl super::layer::Layer<S, Service = Self> + Clone {
        super::layer::mk(move |inner| Self::new(inner, f.clone()))
    }
}

impl<Req, E, S, F: Clone> super::Service<Req> for MapErr<S, F>
where
    S: super::Service<Req>,
    F: FnOnce(S::Error) -> E,
{
    type Response = S::Response;
    type Error = E;
    type Future = futures::future::MapErr<S::Future, F>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), E>> {
        self.inner.poll_ready(cx).map_err(self.f.clone())
    }

    #[inline]
    fn call(&mut self, req: Req) -> Self::Future {
        self.inner.call(req).map_err(self.f.clone())
    }
}

impl<Req, E, P, S, F: Clone> super::Proxy<Req, S> for MapErr<P, F>
where
    P: super::Proxy<Req, S>,
    S: super::Service<P::Request>,
    F: FnOnce(P::Error) -> E,
    E: Into<Error>,
{
    type Request = P::Request;
    type Response = P::Response;
    type Error = E;
    type Future = futures::future::MapErr<P::Future, F>;

    #[inline]
    fn proxy(&self, inner: &mut S, req: Req) -> Self::Future {
        self.inner.proxy(inner, req).map_err(self.f.clone())
    }
}
