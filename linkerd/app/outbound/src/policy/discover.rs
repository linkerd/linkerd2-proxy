use super::{Policy, Receiver};
use linkerd_app_core::{
    cache::Cache,
    svc::{self, stack::Param, NewService},
    transport::OrigDstAddr,
    Error,
};
use std::task::{Context, Poll};
use tokio::time::Duration;

#[derive(Clone)]
pub struct Discover<S, N> {
    cache: Cache<OrigDstAddr, Receiver>,
    inner: S,
    new_svc: N,
}

impl<S: Clone, N> Discover<S, N> {
    pub(crate) fn layer(
        inner: S,
        max_idle_age: Duration,
    ) -> impl svc::Layer<N, Service = Self> + Clone {
        svc::layer::mk(move |new_svc| Self {
            cache: Cache::new(max_idle_age),
            inner: inner.clone(),
            new_svc,
        })
    }
}

impl<S, N, T> svc::Service<T> for Discover<S, N>
where
    T: Param<OrigDstAddr> + Send + 'static,
    S: svc::Service<OrigDstAddr, Response = Receiver> + Clone + Send + 'static,
    S::Future: Send,
    N: NewService<(Policy, T)> + Clone + Send + 'static,
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
        let cache = self.cache.clone();
        let mut inner = self.inner.clone();
        let new_svc = self.new_svc.clone();
        Box::pin(async move {
            // TODO(eliza): we can't just call
            // `cache.get_or_insert_with(inner.call(dst).await)` because
            // `Cache::get_or_insert_with` is synchronous, so we have to
            // await the future separately...it would be nicer to add a
            // `get_or_insert_async` type method...
            let policy = if let Some(policy) = cache.get(&dst) {
                policy
            } else {
                let policy = inner.call(dst).await?;
                cache.get_or_insert_with(dst, |_| policy)
            };
            let policy = Policy { dst, policy };
            Ok(new_svc.new_service((policy, target)))
        })
    }
}
