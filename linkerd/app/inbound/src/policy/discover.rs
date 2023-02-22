use super::{AllowPolicy, GetPolicy, LookupAddr};
use futures::ready;
use linkerd_app_core::{svc, Error};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

#[derive(Debug, Clone)]
pub struct Discover<X, G, N> {
    extract_addr: X,
    get_policy: G,
    new_svc: N,
}

#[pin_project::pin_project]
pub struct DiscoverFuture<T, F, N> {
    target: Option<T>,
    #[pin]
    inner: F,
    new_svc: N,
}

impl<G: GetPolicy + Clone, N> Discover<(), G, N> {
    pub fn layer(get_policy: G) -> impl svc::layer::Layer<N, Service = Self> + Clone {
        Self::layer_via(get_policy, ())
    }
}

impl<X: Clone, G: GetPolicy + Clone, N> Discover<X, G, N> {
    pub fn layer_via(
        get_policy: G,
        extract_addr: X,
    ) -> impl svc::layer::Layer<N, Service = Self> + Clone {
        svc::layer::mk(move |new_svc| Self {
            get_policy: get_policy.clone(),
            new_svc,
            extract_addr: extract_addr.clone(),
        })
    }
}

impl<X, G: GetPolicy, N, T> svc::Service<T> for Discover<X, G, N>
where
    G: GetPolicy,
    N: svc::NewService<(AllowPolicy, T)> + Clone,
    X: svc::ExtractParam<LookupAddr, T>,
{
    type Error = Error;
    type Response = N::Service;
    type Future = DiscoverFuture<T, G::Future, N>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        let dst = self.extract_addr.extract_param(&target);
        DiscoverFuture {
            target: Some(target),
            inner: self.get_policy.get_policy(dst),
            new_svc: self.new_svc.clone(),
        }
    }
}

// === impl DiscoverFuture ===

impl<T, F, N, E> Future for DiscoverFuture<T, F, N>
where
    F: Future<Output = Result<AllowPolicy, E>>,
    N: svc::NewService<(AllowPolicy, T)>,
{
    type Output = Result<N::Service, E>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let policy = ready!(this.inner.poll(cx))?;
        let svc = this
            .new_svc
            .new_service((policy, this.target.take().expect("polled after ready")));
        Poll::Ready(Ok(svc))
    }
}
