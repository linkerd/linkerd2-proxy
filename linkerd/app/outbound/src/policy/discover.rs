use super::Receiver;
use linkerd_app_core::{
    svc::{self, stack::Param, NewService},
    transport::OrigDstAddr,
    Error,
};
use std::task::{Context, Poll};
#[derive(Clone)]
pub struct Discover<S, N> {
    inner: S,
    new_svc: N,
}

impl<S: Clone, N> Discover<S, N> {
    pub(crate) fn layer(inner: S) -> impl svc::Layer<N, Service = Self> + Clone {
        svc::layer::mk(move |new_svc| Self {
            inner: inner.clone(),
            new_svc,
        })
    }
}

impl<S, N, T> svc::Service<T> for Discover<S, N>
where
    T: Param<OrigDstAddr> + Send + 'static,
    S: svc::Service<OrigDstAddr, Response = Receiver> + Send + 'static,
    S::Future: Send,
    N: NewService<(Receiver, T)> + Clone + Send + 'static,
    Error: From<S::Error>,
{
    type Response = N::Service;
    type Error = Error;
    type Future = futures::future::BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Error::from)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let dst = target.param();
        let lookup = self.inner.call(dst);
        let new_svc = self.new_svc.clone();
        Box::pin(async move {
            let policy = lookup.await?;
            Ok(new_svc.new_service((policy, target)))
        })
    }
}
