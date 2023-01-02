use super::{params, NewDistribute};
use linkerd_stack::{NewService, Param};
use parking_lot::Mutex;
use std::{fmt::Debug, hash::Hash, sync::Arc};

/// A [`NewService`] that produces [`NewDistribute`]s using a shared cache of
/// backends.
#[derive(Debug)]
pub struct CacheNewDistribute<T, A, N, S> {
    inner: Arc<Inner<T, A, N, S>>,
}

#[derive(Debug)]
pub struct Inner<T, A, N, S> {
    target: T,
    new_backend: N,
    backends: Mutex<ahash::AHashMap<A, S>>,
}

// === impl CacheNewDistribute ===

impl<P, T, A, N> NewService<P> for CacheNewDistribute<T, A, N, N::Service>
where
    T: Clone,
    A: Eq + Hash + Clone,
    P: Param<params::BackendAddrs<A>>,
    N: NewService<(A, T)>,
    N::Service: Clone,
{
    type Service = NewDistribute<A, N::Service>;

    fn new_service(&self, p: P) -> Self::Service {
        let params::BackendAddrs(addrs) = p.param();

        let mut cache = self.inner.backends.lock();

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

                let backend = self
                    .inner
                    .new_backend
                    .new_service(((addr.clone()), self.inner.target.clone()));
                cache.insert(addr.clone(), backend);
            }
        }

        NewDistribute::from(cache.clone())
    }
}

impl<T, A, N, S> Clone for CacheNewDistribute<T, A, N, S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}
