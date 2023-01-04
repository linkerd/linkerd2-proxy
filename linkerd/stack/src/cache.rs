use crate::{layer, NewService};
use ahash::AHashMap;
use parking_lot::Mutex;
use std::{fmt::Debug, hash::Hash, marker::PhantomData, sync::Arc};

/// A [`NewService`] that lazy builds stacks for each `K`-typed key.
#[derive(Clone, Debug)]
pub struct NewCache<K, N> {
    inner: N,
    _marker: PhantomData<fn(K)>,
}

/// A [`NewService`]
#[derive(Clone, Debug)]
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

    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(Self::new)
    }
}

impl<T, K, N, M> NewService<T> for NewCache<K, N>
where
    T: Clone,
    N: NewService<T, Service = M>,
    M: NewService<K>,
{
    type Service = Cache<K, M, M::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        Cache::new(self.inner.new_service(target))
    }
}

// === impl Cache ===

impl<K, N, S> Cache<K, N, S> {
    pub fn new(new_inner: N) -> Self {
        Self {
            new_inner,
            services: Default::default(),
        }
    }

    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(Self::new)
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
