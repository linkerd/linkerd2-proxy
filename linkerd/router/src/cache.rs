use crate::NewService;
use ahash::AHashMap;
use parking_lot::Mutex;
use std::{fmt::Debug, hash::Hash, marker::PhantomData, sync::Arc};

/// A [`NewService`] that produces [`Cache`]s.
#[derive(Debug)]
pub struct NewCache<K, N> {
    inner: N,
    _marker: PhantomData<fn(K)>,
}

/// A [`NewService`] that lazily builds an inner `S`-typed service for each
/// `K`-typed target.
///
/// An inner service is built once for each `K`-typed target. The inner service
/// is then cloned for each `K` value. It is not dropped until all clones of the
/// cache are dropped.
#[derive(Debug)]
pub struct Cache<K, N, S> {
    new_inner: N,
    services: Arc<Mutex<AHashMap<K, S>>>,
}

// === impl NewCache ===

impl<K, N> NewCache<K, N> {
    pub fn new(inner: N) -> Self {
        Self {
            inner,
            _marker: PhantomData,
        }
    }
}

impl<T, K, N, M> NewService<T> for NewCache<K, N>
where
    T: Clone,
    N: NewService<T, Service = M>,
    M: NewService<K>,
{
    type Service = Cache<K, M, M::Service>;

    #[inline]
    fn new_service(&self, target: T) -> Self::Service {
        Cache::new(self.inner.new_service(target))
    }
}

impl<K, N: Clone> Clone for NewCache<K, N> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _marker: self._marker,
        }
    }
}

// === impl Cache ===

impl<K, N, S> Cache<K, N, S> {
    pub(super) fn new(new_inner: N) -> Self {
        Self {
            new_inner,
            services: Default::default(),
        }
    }
}

impl<K, N> NewService<K> for Cache<K, N, N::Service>
where
    K: Eq + Hash + Clone,
    N: NewService<K>,
    N::Service: Clone,
{
    type Service = N::Service;

    fn new_service(&self, key: K) -> Self::Service {
        self.services
            .lock()
            .entry(key.clone())
            .or_insert_with(|| self.new_inner.new_service(key))
            .clone()
    }
}

impl<K, N: Clone, S> Clone for Cache<K, N, S> {
    fn clone(&self) -> Self {
        Self {
            new_inner: self.new_inner.clone(),
            services: self.services.clone(),
        }
    }
}
