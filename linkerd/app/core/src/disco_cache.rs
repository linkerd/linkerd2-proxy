use futures::TryFutureExt;
use linkerd_error::Error;
use linkerd_idle_cache::{Cached, NewIdleCached};
use linkerd_stack::{
    layer, queue, CloneParam, FutureService, MapErrBoxed, NewQueueWithoutTimeout, NewService,
    Oneshot, Param, QueueWithoutTimeout, Service, ServiceExt, ThunkClone,
};
use std::{fmt, hash::Hash, task, time};

/// A `NewService` that extracts a `K`-typed key from each target to build a
/// `CachedDiscovery`. The key is passed to the `D` type `NewThunk`
#[derive(Clone)]
pub struct NewDiscoveryCache<K, D, N>
where
    K: Clone + fmt::Debug + Eq + Hash + Send + Sync + 'static,
    D: Service<K, Error = Error> + Clone + Send + Sync + 'static,
    D::Response: Clone + Send + Sync,
    D::Future: Send + Unpin,
{
    inner: N,
    cache: NewIdleCached<K, NewQueueDiscoverThunk<D>>,
}

#[derive(Debug)]
#[pin_project::pin_project]
pub struct DiscoverFuture<D: Service<()>, N> {
    inner: N,
    cached: Cached<D>,
    #[pin]
    future: Oneshot<Cached<D>, ()>,
}

#[derive(Clone, Debug)]
struct NewDiscoverThunk<D> {
    discover: D,
}

type NewQueueDiscoverThunk<D> = NewQueueThunk<NewDiscoverThunk<D>>;
type NewQueueThunk<D> = NewQueueWithoutTimeout<CloneParam<queue::Capacity>, (), D>;

impl<K, D, N> NewDiscoveryCache<K, D, N>
where
    K: Clone + fmt::Debug + Eq + Hash + Send + Sync + 'static,
    D: Service<K, Error = Error> + Clone + Send + Sync + 'static,
    D::Response: Clone + Send + Sync,
    D::Future: Send + Unpin,
{
    pub fn new(inner: N, discover: D, timeout: time::Duration, capacity: usize) -> Self {
        let cache = {
            let thunk = NewDiscoverThunk { discover };
            let queue =
                NewQueueWithoutTimeout::new(thunk, CloneParam::from(queue::Capacity(capacity)));
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
    D::Future: Send + Unpin,
    M: NewService<T, Service = N> + Clone,
    N: NewService<D::Response> + Clone + Send + 'static,
{
    type Service = DiscoveryCache<D::Response, N, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let key = target.param();
        let cached = self.cache.new_service(key);
        let inner = self.inner.new_service(target);
        let future = cached.clone().oneshot(());
        FutureService::new(DiscoverFuture {
            future,
            cached,
            inner,
        })
    }
}

pub type DiscoveryCache<D, N, S> =
    FutureService<DiscoverFuture<QueueWithoutTimeout<(), D>, N>, Cached<S>>;

impl<T, D> NewService<T> for NewDiscoverThunk<D>
where
    D: Service<T, Error = Error> + Clone,
    D::Response: Clone,
    D::Future: Unpin,
{
    type Service = DiscoverThunk<T, D::Response, D>;

    fn new_service(&self, target: T) -> Self::Service {
        let discover = self.discover.clone();
        FutureService::new(
            discover
                .oneshot(target)
                .map_ok(|discovery| MapErrBoxed::new(ThunkClone::new(discovery), ())),
        )
    }
}

pub type DiscoverThunk<Req, Rsp, S> = FutureService<
    futures::future::MapOk<Oneshot<S, Req>, fn(Rsp) -> MapErrBoxed<ThunkClone<Rsp>>>,
    MapErrBoxed<ThunkClone<Rsp>>,
>;

impl<D, N> std::future::Future for DiscoverFuture<D, N>
where
    D: Service<()>,
    N: NewService<D::Response>,
{
    type Output = Result<Cached<N::Service>, D::Error>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Self::Output> {
        let this = self.project();
        let discovery = futures::ready!(this.future.poll(cx))?;
        let inner = this.inner.new_service(discovery);
        let cached = this.cached.clone_with(inner);
        task::Poll::Ready(Ok(cached))
    }
}
