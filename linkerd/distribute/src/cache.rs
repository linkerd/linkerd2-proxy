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
pub struct BackendCache<K, N, S>(Arc<Inner<K, N, S>>);

#[derive(Debug)]
struct Inner<K, N, S> {
    new_backend: N,
    backends: Mutex<ahash::AHashMap<K, S>>,
}

// === impl BackendCache ===

impl<K, N, S> BackendCache<K, N, S> {
    pub fn new(new_backend: N) -> Self {
        Self(Arc::new(Inner {
            new_backend,
            backends: Default::default(),
        }))
    }

    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(Self::new)
    }
}

impl<T, K, N> NewService<T> for BackendCache<K, N, N::Service>
where
    T: Param<params::Backends<K>>,
    K: Eq + Hash + Clone + std::fmt::Debug,
    N: NewService<K>,
    N::Service: Clone,
{
    type Service = NewDistribute<K, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let params::Backends(backends) = target.param();

        let mut cache = self.0.backends.lock();

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
                cache.insert(
                    backend.clone(),
                    self.0.new_backend.new_service(backend.clone()),
                );
            }
        }

        NewDistribute::from(cache.clone())
    }
}

impl<K, N, S> Clone for BackendCache<K, N, S> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
