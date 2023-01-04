use super::{params, NewDistribute};
use linkerd_stack::{layer, NewService, Param};
use parking_lot::Mutex;
use std::{fmt::Debug, hash::Hash};

/// A [`NewService`] that produces [`NewDistribute`]s using a shared cache of
/// backends.
#[derive(Debug)]
pub struct CacheNewDistribute<K, N, S>(Inner<K, N, S>);

#[derive(Debug)]
struct Inner<K, N, S> {
    new_backend: N,
    backends: Mutex<ahash::AHashMap<K, S>>,
}

// === impl CacheNewDistribute ===

impl<K, N, S> CacheNewDistribute<K, N, S> {
    pub fn new(new_backend: N) -> Self {
        Self(Inner {
            new_backend,
            backends: Mutex::new(ahash::AHashMap::new()),
        })
    }

    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(Self::new)
    }
}

impl<T, K, N> NewService<T> for CacheNewDistribute<K, N, N::Service>
where
    T: Param<params::Backends<K>>,
    K: Eq + Hash + Clone,
    N: NewService<K>,
    N::Service: Clone,
{
    type Service = NewDistribute<K, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let params::Backends(addrs) = target.param();

        let mut cache = self.0.backends.lock();

        // Remove all backends that aren't in the updated set of addrs.
        cache.retain(|addr, _| addrs.contains(addr));

        // If there are additional addrs, build new services for each and add
        // them to the cache.
        debug_assert!(addrs.len() >= cache.len());
        if addrs.len() > cache.len() {
            cache.reserve(addrs.len());
            for addr in &*addrs {
                // Skip rebuilding targets we already have a stack for.
                if cache.contains_key(addr) {
                    continue;
                }

                let backend = self.0.new_backend.new_service(addr.clone());
                cache.insert(addr.clone(), backend);
            }
        }

        NewDistribute::from(cache.clone())
    }
}

// impl<K, N, S> Clone for CacheNewDistribute<K, N, S> {
//     fn clone(&self) -> Self {
//         Self(self.0.clone())
//     }
// }
