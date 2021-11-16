use futures::TryFutureExt;
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
