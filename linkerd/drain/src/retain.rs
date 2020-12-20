use crate::Watch;
use linkerd2_stack::{layer, NewService};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

#[derive(Clone, Debug)]
pub struct NewRetain<N> {
    inner: N,
    drain: Watch,
}

#[derive(Clone, Debug)]
pub struct Retain<S> {
    inner: S,
    drain: Watch,
}

impl<N> NewRetain<N> {
    pub fn new(drain: Watch, inner: N) -> Self {
        Self { drain, inner }
    }

    pub fn layer(drain: Watch) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(drain.clone(), inner))
    }
}

impl<T, N: NewService<T>> NewService<T> for NewRetain<N> {
    type Service = Retain<N::Service>;

    fn new_service(&mut self, target: T) -> Self::Service {
        let inner = self.inner.new_service(target);
        Retain {
            inner,
            drain: self.drain.clone(),
        }
    }
}

impl<Req, S> tower::Service<Req> for Retain<S>
where
    S: tower::Service<Req>,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, S::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        Box::pin(
            self.drain
                .clone()
                .ignore_signal()
                .release_after(self.inner.call(req)),
        )
    }
}
