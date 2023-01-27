#![allow(warnings)] // FIXME

use linkerd_idle_cache::{Cached, NewIdleCached};
use linkerd_stack::{layer, NewService, Param, Service};
use std::{
    fmt,
    hash::Hash,
    task::{Context, Poll},
};
use tokio::time;

#[derive(Clone)]
pub struct NewDiscoveryCache<K, N>
where
    K: Eq + Hash,
    N: NewService<K>,
    N::Service: Clone,
{
    cache: NewIdleCached<K, N>,
}

#[derive(Clone, Debug)]
pub struct DiscoveryCache<T, S> {
    target: T,
    thunk: Cached<S>,
}

impl<K, N> NewDiscoveryCache<K, N>
where
    K: Clone + fmt::Debug + Eq + Hash + Send + Sync + 'static,
    N: NewService<K> + 'static,
    N::Service: Clone + Send + Sync + 'static,
{
    pub fn new(inner: N, idle: time::Duration) -> Self {
        Self {
            cache: NewIdleCached::new(inner, idle),
        }
    }

    pub fn layer(idle: time::Duration) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(inner, idle))
    }
}

impl<T, K, N> NewService<T> for NewDiscoveryCache<K, N>
where
    T: Param<K>,
    K: Clone + fmt::Debug + Eq + Hash + Send + Sync + 'static,
    N: NewService<K> + 'static,
    N::Service: Service<()> + Clone + Send + Sync + 'static,
{
    type Service = DiscoveryCache<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let key = target.param();
        let thunk = self.cache.new_service(key);
        DiscoveryCache { thunk }
    }
}

impl<Req, S> Service<Req> for DiscoveryCache<S>
where
    S: Service<()>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // TODO(ver): Call the thunk, get the cloned response value, build a new
        // inner serice based on the cloned response and the original target.
        self.thunk.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        self.inner.call(req)
    }
}
