use super::{params, NewDistribute};
use linkerd_stack::{layer, NewService, Param};
use parking_lot::Mutex;
use std::{fmt::Debug, hash::Hash, sync::Arc};

/// A [`NewService`] that produces [`NewDistribute`]s using a shared cache of
/// backends.
///
/// On each call to [`NewService::new_service`], the cache extracts a new set of
/// [`params::Backends`] from the target to determine which
/// services should be added/removed from the cache.
#[derive(Debug)]
pub struct BackendCache<K, N, S> {
    inner: N,
    backends: Arc<Mutex<ahash::AHashMap<K, S>>>,
}

// === impl BackendCache ===

impl<K, N, S> BackendCache<K, N, S> {
    pub fn new(inner: N) -> Self {
        Self {
            inner,
            backends: Default::default(),
        }
    }

    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(Self::new)
    }
}

impl<T, K, N, NewBk, S> NewService<T> for BackendCache<K, N, S>
where
    T: Param<params::Backends<K>>,
    K: Eq + Hash + Clone + Debug,
    N: NewService<T, Service = NewBk>,
    NewBk: NewService<K, Service = S>,
    S: Clone,
{
    type Service = NewDistribute<K, S>;

    fn new_service(&self, target: T) -> Self::Service {
        let params::Backends(backends) = target.param();

        let new_backend = self.inner.new_service(target);
        let mut cache = self.backends.lock();

        // Remove all backends that aren't in the updated set of addrs.
        cache.retain(|backend, _| {
            if backends.contains(backend) {
                true
            } else {
                tracing::debug!(?backend, "Removing");
                false
            }
        });

        // If there are additional addrs, cache a new service for each.
        debug_assert!(backends.len() >= cache.len());
        if backends.len() > cache.len() {
            cache.reserve(backends.len());
            for backend in &*backends {
                // Skip rebuilding targets we already have a stack for.
                if cache.contains_key(backend) {
                    tracing::trace!(?backend, "Retaining");
                    continue;
                }

                tracing::debug!(?backend, "Adding");
                cache.insert(backend.clone(), new_backend.new_service(backend.clone()));
            }
        }

        NewDistribute::from(cache.clone())
    }
}

impl<K, N: Clone, S> Clone for BackendCache<K, N, S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            backends: self.backends.clone(),
        }
    }
}
