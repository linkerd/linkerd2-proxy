//! A specialized `NewIdleCache` to manage discovery stae, usually from a
//! control plane client.

use futures::TryFutureExt;
use linkerd_error::Error;
use linkerd_idle_cache::{Cached, NewIdleCached};
use linkerd_stack::{
    layer, queue, CloneParam, ExtractParam, FutureService, MapErrBoxed, NewQueueWithoutTimeout,
    NewService, Oneshot, QueueWithoutTimeout, Service, ServiceExt, ThunkClone,
};
use std::{fmt, hash::Hash, task, time};

/// A [`NewService`] that extracts a `K`-typed key from each target to build a
/// [`Cached`]<[`DiscoverThunk`]>.
#[derive(Clone)]
pub struct NewCachedDiscover<K, X, D, N>
where
    K: Clone + fmt::Debug + Eq + Hash + Send + Sync + 'static,
    D: Service<K, Error = Error> + Clone + Send + Sync + 'static,
    D::Response: Clone + Send + Sync,
    D::Future: Send + Unpin,
{
    // NewService<K, Service<(), D::Response>>
    cache: NewIdleCached<K, NewQueueThunk<NewDiscoverThunk<D>>>,

    // NewService<D::Response>
    inner: N,

    /// Extracts `K`-typed keys.
    extract_key: X,
}

/// The future that drives discovery to build an new inner service wrapped
/// in the [`Cached`] decorator from the discovery lookup, preventing the
/// cache's idle timeout from starting until returned services are dropped.
#[derive(Debug)]
#[pin_project::pin_project]
pub struct CachedDiscoverFuture<D: Service<()>, N> {
    // NewService<D::Response>
    inner: N,

    // Holds a cache handle so that we can carry that forward with the returned
    // service.
    cached: Cached<D>,

    // A future that obtains a `D::Response` and produces an `N::Service`.
    #[pin]
    future: Oneshot<Cached<D>, ()>,
}

/// A [`Service<()>`] that uses a `D`-typed discovery service to build a new
/// inner service wrapped in the discovery service's cache handle.
pub type CachedDiscover<D, N, S> = FutureService<CachedDiscoverFuture<QueueThunk<D>, N>, Cached<S>>;

/// A [`Service<()>`] that discovers a `Rsp` and returns a clone of it for each
/// call.
pub type DiscoverThunk<Req, Rsp, S> = FutureService<
    futures::future::MapOk<Oneshot<S, Req>, fn(Rsp) -> MapErrBoxed<ThunkClone<Rsp>>>,
    MapErrBoxed<ThunkClone<Rsp>>,
>;

#[derive(Clone, Debug)]
struct NewDiscoverThunk<D> {
    discover: D,
}

// We do not enforce any timeouts on discovery. Nor are we concerned with load
// shedding. `NewCachedDiscover` returns a `FutureService`, so the internal
// queue's capacity can exert backpressure into `Service::poll_ready`. This is
// a good thing. That service stack can determine its own load shedding and
// failfast semantics independently. The queue capacity is purely to avoid
// contention across clones.
type NewQueueThunk<D> = NewQueueWithoutTimeout<CloneParam<queue::Capacity>, (), D>;
type QueueThunk<D> = QueueWithoutTimeout<(), D>;
const QUEUE_CAPACITY: queue::Capacity = queue::Capacity(10);

// === impl NewCachedDiscover ===

impl<K, D, N> NewCachedDiscover<K, (), D, N>
where
    K: Clone + fmt::Debug + Eq + Hash + Send + Sync + 'static,
    D: Service<K, Error = Error> + Clone + Send + Sync + 'static,
    D::Response: Clone + Send + Sync,
    D::Future: Send + Unpin,
{
    pub fn new(inner: N, discover: D, timeout: time::Duration) -> Self {
        Self::new_via(inner, discover, timeout, ())
    }

    pub fn layer(disco: D, idle: time::Duration) -> impl layer::Layer<N, Service = Self> + Clone {
        Self::layer_via(disco, idle, ())
    }
}

impl<K, X, D, N> NewCachedDiscover<K, X, D, N>
where
    K: Clone + fmt::Debug + Eq + Hash + Send + Sync + 'static,
    D: Service<K, Error = Error> + Clone + Send + Sync + 'static,
    D::Response: Clone + Send + Sync,
    D::Future: Send + Unpin,
{
    pub fn new_via(inner: N, discover: D, timeout: time::Duration, extract_key: X) -> Self {
        let queue = NewQueueThunk::new(
            NewDiscoverThunk { discover },
            CloneParam::from(QUEUE_CAPACITY),
        );
        Self {
            inner,
            cache: NewIdleCached::new(queue, timeout),
            extract_key,
        }
    }

    pub fn layer_via(
        disco: D,
        idle: time::Duration,
        extract_key: X,
    ) -> impl layer::Layer<N, Service = Self> + Clone
    where
        X: Clone,
    {
        layer::mk(move |inner| Self::new_via(inner, disco.clone(), idle, extract_key.clone()))
    }
}

impl<T, K, X, D, M, N> NewService<T> for NewCachedDiscover<K, X, D, M>
where
    X: ExtractParam<K, T>,
    T: Clone,
    K: Clone + fmt::Debug + Eq + Hash + Send + Sync + 'static,
    D: Service<K, Error = Error> + Clone + Send + Sync + 'static,
    D::Response: Clone + Send + Sync + 'static,
    D::Future: Send + Unpin,
    M: NewService<T, Service = N> + Clone,
    N: NewService<D::Response> + Clone + Send + 'static,
{
    type Service = CachedDiscover<D::Response, N, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let key = self.extract_key.extract_param(&target);
        let cached = self.cache.new_service(key);
        let inner = self.inner.new_service(target);
        let future = cached.clone().oneshot(());
        FutureService::new(CachedDiscoverFuture {
            future,
            cached,
            inner,
        })
    }
}

// === impl NewDiscoverThunk ===

impl<T, D> NewService<T> for NewDiscoverThunk<D>
where
    D: Service<T, Error = Error> + Clone,
    D::Response: Clone,
    D::Future: Unpin,
{
    type Service = DiscoverThunk<T, D::Response, D>;

    fn new_service(&self, target: T) -> Self::Service {
        let disco = self.discover.clone().oneshot(target);
        FutureService::new(disco.map_ok(|rsp| ThunkClone::new(rsp).into()))
    }
}

// === impl DiscoverFUture ===

impl<D, N> std::future::Future for CachedDiscoverFuture<D, N>
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
