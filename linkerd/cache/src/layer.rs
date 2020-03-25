use crate::Cache;
use crate::Handle;
use futures::Poll;
use linkerd2_stack::NewService;

pub struct CacheLayer<T, L> {
    track_layer: L,
    _marker: std::marker::PhantomData<fn(T)>,
}

// === impl CacheLayer ===

impl<T, L> CacheLayer<T, L> {
    pub fn new(track_layer: L) -> Self {
        Self {
            track_layer,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<L: Clone, T> Clone for CacheLayer<T, L> {
    fn clone(&self) -> Self {
        Self::new(self.track_layer.clone())
    }
}

impl<T, L, N> tower::layer::Layer<N> for CacheLayer<T, L>
where
    T: Eq + std::hash::Hash,
    N: NewService<T> + Clone,
    L: tower::layer::Layer<Track<N>> + Clone,
    L::Service: NewService<T>,
{
    type Service = Cache<T, NewTrack<L, N>>;

    fn layer(&self, inner: N) -> Self::Service {
        let layer = self.track_layer.clone();
        Cache::new(NewTrack { inner, layer })
    }
}

#[derive(Clone)]
pub struct NewTrack<L, N> {
    layer: L,
    inner: N,
}

#[derive(Clone)]
pub struct Track<T> {
    inner: T,
    _handle: Handle,
}

impl<T, L, N: NewService<T>> NewService<(T, Handle)> for NewTrack<L, N>
where
    N: NewService<T> + Clone,
    L: tower::layer::Layer<Track<N>>,
    L::Service: NewService<T>,
{
    type Service = <L::Service as NewService<T>>::Service;

    fn new_service(&self, (target, _handle): (T, Handle)) -> Self::Service {
        let inner = Track {
            _handle,
            inner: self.inner.clone(),
        };
        self.layer.layer(inner).new_service(target)
    }
}

impl<T, N: NewService<T>> NewService<T> for Track<N> {
    type Service = Track<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        Self::Service {
            _handle: self._handle.clone(),
            inner: self.inner.new_service(target),
        }
    }
}

impl<T, S: tower::Service<T>> tower::Service<T> for Track<S> {
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, req: T) -> Self::Future {
        self.inner.call(req)
    }
}
