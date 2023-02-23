use super::{AllowPolicy, GetPolicy};
use futures::ready;
use linkerd_app_core::{svc, transport::OrigDstAddr, Error};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

#[derive(Debug, Clone)]
pub struct Discover<G, N> {
    get_policy: G,
    new_svc: N,
}

#[pin_project::pin_project]
pub struct DiscoverFuture<F, N> {
    #[pin]
    inner: F,
    new_svc: N,
}

impl<G: GetPolicy + Clone, N> Discover<G, N> {
    pub fn layer(get_policy: G) -> impl svc::layer::Layer<N, Service = Self> + Clone {
        svc::layer::mk(move |new_svc| Self {
            get_policy: get_policy.clone(),
            new_svc,
        })
    }
}

impl<G: GetPolicy, N, NSvc, T> svc::Service<T> for Discover<G, N>
where
    G: GetPolicy,
    N: svc::NewService<T, Service = NSvc> + Clone,
    NSvc: svc::NewService<AllowPolicy>,
    T: svc::Param<OrigDstAddr>,
{
    type Error = Error;
    type Response = NSvc::Service;
    type Future = DiscoverFuture<G::Future, NSvc>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        let dst = target.param();
        DiscoverFuture {
            inner: self.get_policy.get_policy(dst),
            new_svc: self.new_svc.new_service(target),
        }
    }
}

// === impl DiscoverFuture ===

impl<F, N, E> Future for DiscoverFuture<F, N>
where
    F: Future<Output = Result<AllowPolicy, E>>,
    N: svc::NewService<AllowPolicy>,
{
    type Output = Result<N::Service, E>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let policy = ready!(this.inner.poll(cx))?;
        let svc = this.new_svc.new_service(policy);
        Poll::Ready(Ok(svc))
    }
}
