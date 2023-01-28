use futures::TryFutureExt;
use linkerd_error::Error;
use linkerd_idle_cache::{Cached, NewIdleCached};
use linkerd_stack::{
    layer, queue, BoxFutureService, CloneParam, FutureService, MapErrBoxed, NewQueueForever,
    NewService, Param, Service, ServiceExt, ThunkClone,
};
use std::{fmt, hash::Hash};
use tokio::time;

/// A `NewService` that extracts a `K`-typed key from each target to build a
/// `CachedDiscovery`. The key is passed to the `D` type `NewThunk`
#[derive(Clone)]
pub struct NewDiscoveryCache<K, D, N>
where
    K: Clone + fmt::Debug + Eq + Hash + Send + Sync + 'static,
    D: Service<K, Error = Error> + Clone + Send + Sync + 'static,
    D::Response: Clone + Send + Sync,
    D::Future: Send,
{
    inner: N,
    cache: NewIdleCached<K, NewQueueThunkCache<D>>,
}

#[derive(Clone, Debug)]
struct NewDiscoverThunk<D> {
    discover: D,
}

type NewQueueThunk<D> = NewQueueForever<CloneParam<queue::Capacity>, (), D>;

type NewQueueThunkCache<D> = NewQueueThunk<NewDiscoverThunk<D>>;

impl<K, D, N> NewDiscoveryCache<K, D, N>
where
    K: Clone + fmt::Debug + Eq + Hash + Send + Sync + 'static,
    D: Service<K, Error = Error> + Clone + Send + Sync + 'static,
    D::Response: Clone + Send + Sync + 'static,
    D::Future: Send + 'static,
{
    pub fn new(inner: N, discover: D, timeout: time::Duration, capacity: usize) -> Self {
        let cache = {
            let thunk = NewDiscoverThunk { discover };
            let queue = NewQueueForever::new(thunk, CloneParam::from(queue::Capacity(capacity)));
            NewIdleCached::new(queue, timeout)
        };
        Self { inner, cache }
    }

    pub fn layer(
        disco: D,
        idle: time::Duration,
        capacity: usize,
    ) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(inner, disco.clone(), idle, capacity))
    }
}

impl<T, K, D, M, N> NewService<T> for NewDiscoveryCache<K, D, M>
where
    T: Param<K> + Clone,
    K: Clone + fmt::Debug + Eq + Hash + Send + Sync + 'static,
    D: Service<K, Error = Error> + Clone + Send + Sync + 'static,
    D::Response: Clone + Send + Sync + 'static,
    D::Future: Send + 'static,
    M: NewService<T, Service = N> + Clone,
    N: NewService<D::Response> + Clone + Send + Sync + 'static,
{
    type Service = DiscoveryCache<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let key = target.param();
        let cached = self.cache.new_service(key);
        let inner = self.inner.new_service(target);
        let discover = cached.clone();
        FutureService::new(Box::pin(
            discover
                .oneshot(())
                .map_ok(move |discovery| cached.clone_with(inner.new_service(discovery))),
        ))
    }
}

pub type DiscoveryCache<S> = BoxFutureService<Cached<S>>;

impl<T, D> NewService<T> for NewDiscoverThunk<D>
where
    T: Send + 'static,
    D: Service<T, Error = Error> + Clone + Send + Sync + 'static,
    D::Response: Clone + Send + Sync + 'static,
    D::Future: Send + 'static,
{
    type Service = DiscoverThunk<D::Response>;

    fn new_service(&self, target: T) -> Self::Service {
        let discover = self.discover.clone();
        FutureService::new(Box::pin(
            discover
                .oneshot(target)
                .map_ok(|discovery| MapErrBoxed::new(ThunkClone::new(discovery), ())),
        ))
    }
}

pub type DiscoverThunk<Rsp> = BoxFutureService<MapErrBoxed<ThunkClone<Rsp>>>;
