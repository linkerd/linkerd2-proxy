#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

use ahash::AHashMap as HashMap;
use linkerd_stack::{layer, NewService};
use parking_lot::RwLock;
use std::{
    collections::hash_map::Entry,
    hash::Hash,
    sync::{Arc, Weak},
    task::{Context, Poll},
};
use tokio::{sync::Notify, time};
use tracing::{debug, instrument, trace};

#[derive(Clone)]
pub struct Cache<T, N>
where
    T: Eq + Hash,
    N: NewService<T>,
{
    inner: N,
    services: Arc<Services<T, N::Service>>,
    idle: time::Duration,
}

#[derive(Clone, Debug)]
pub struct Cached<S>
where
    S: Send + Sync + 'static,
{
    inner: S,
    // Notifies entry's eviction task that a drop has occurred.
    handle: Arc<Notify>,
}

type Services<T, S> = RwLock<HashMap<T, (S, Weak<Notify>)>>;

// === impl Cache ===

impl<T, N> Cache<T, N>
where
    T: Clone + std::fmt::Debug + Eq + Hash + Send + Sync + 'static,
    N: NewService<T> + 'static,
    N::Service: Send + Sync + 'static,
{
    pub fn layer(idle: time::Duration) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(idle, inner))
    }

    fn new(idle: time::Duration, inner: N) -> Self {
        let services = Arc::new(Services::default());
        Self {
            inner,
            services,
            idle,
        }
    }

    fn spawn_idle(
        target: T,
        idle: time::Duration,
        cache: &Arc<Services<T, N::Service>>,
    ) -> Arc<Notify> {
        // Spawn a background task that holds the handle. Every time the handle
        // is notified, it resets the idle timeout. Every time teh idle timeout
        // expires, the handle is checked and the service is dropped if there
        // are no active handles.
        let handle = Arc::new(Notify::new());
        tokio::spawn(Self::evict(
            target,
            idle,
            handle.clone(),
            Arc::downgrade(cache),
        ));
        handle
    }

    #[instrument(level = "debug", skip(idle, reset, cache))]
    async fn evict(
        target: T,
        idle: time::Duration,
        mut reset: Arc<Notify>,
        cache: Weak<Services<T, N::Service>>,
    ) {
        // Wait for the handle to be notified before starting to track idleness.
        reset.notified().await;
        debug!("Awaiting idleness");

        // Wait for either the reset to be notified or the idle timeout to
        // elapse.
        loop {
            tokio::select! {
                biased;

                // If the reset was notified, restart the timer.
                _ = reset.notified() => {
                    trace!("Reset");
                }
                _ = time::sleep(idle) => match cache.upgrade() {
                    Some(cache) => match Arc::try_unwrap(reset) {
                        // If this is the last reference to the handle after the
                        // idle timeout, remove the cache entry.
                        Ok(_) => {
                            let removed = cache.write().remove(&target).is_some();
                            debug_assert!(removed, "Cache item must exist: {:?}", target);
                            debug!("Cache entry dropped");
                            return;
                        }
                        // Otherwise, another handle has been acquired, so
                        // restore our reset reference for the next iteration.
                        Err(r) => {
                            trace!("The handle is still active");
                            reset = r;
                        }
                    },
                    None => {
                        trace!("Cache already dropped");
                        return;
                    }
                },
            }
        }
    }
}

impl<T, N> NewService<T> for Cache<T, N>
where
    T: Clone + std::fmt::Debug + Eq + Hash + Send + Sync + 'static,
    N: NewService<T> + 'static,
    N::Service: Clone + Send + Sync + 'static,
{
    type Service = Cached<N::Service>;

    fn new_service(&self, target: T) -> Cached<N::Service> {
        // We expect the item to be available in most cases, so initially obtain
        // only a read lock.
        if let Some((svc, weak)) = self.services.read().get(&target) {
            if let Some(handle) = weak.upgrade() {
                trace!("Using cached service");
                return Cached {
                    inner: svc.clone(),
                    handle,
                };
            }
        }

        // Otherwise, obtain a write lock to insert a new service.
        match self.services.write().entry(target.clone()) {
            Entry::Occupied(mut entry) => {
                // Another thread raced us to create a service for this target.
                // Try to use it.
                let (svc, weak) = entry.get();
                match weak.upgrade() {
                    Some(handle) => {
                        trace!(?target, "Using cached service");
                        Cached {
                            inner: svc.clone(),
                            handle,
                        }
                    }
                    None => {
                        debug!(?target, "Replacing defunct service");
                        let handle = Self::spawn_idle(target.clone(), self.idle, &self.services);
                        let inner = self.inner.new_service(target);
                        entry.insert((inner.clone(), Arc::downgrade(&handle)));
                        Cached { inner, handle }
                    }
                }
            }
            Entry::Vacant(entry) => {
                debug!(?target, "Caching new service");
                let handle = Self::spawn_idle(target.clone(), self.idle, &self.services);
                let inner = self.inner.new_service(target);
                entry.insert((inner.clone(), Arc::downgrade(&handle)));
                Cached { inner, handle }
            }
        }
    }
}

// === impl Cached ===

impl<Req, S> tower::Service<Req> for Cached<S>
where
    S: tower::Service<Req> + Send + Sync + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, req: Req) -> Self::Future {
        self.inner.call(req)
    }
}

impl<S> Drop for Cached<S>
where
    S: Send + Sync + 'static,
{
    fn drop(&mut self) {
        self.handle.notify_one();
    }
}

#[cfg(test)]
#[tokio::test(flavor = "current_thread")]
async fn test_idle_retain() {
    time::pause();

    let idle = time::Duration::from_secs(10);
    let cache = Arc::new(Services::default());

    let handle = Cache::<(), fn(()) -> ()>::spawn_idle((), idle, &cache);
    cache.write().insert((), ((), Arc::downgrade(&handle)));
    let c0 = Cached { inner: (), handle };

    let handle = Arc::downgrade(&c0.handle);

    // Let an idle timeout elapse and ensured the held service has not been
    // evicted.
    time::sleep(idle * 2).await;
    assert!(handle.upgrade().is_some());
    assert!(cache.read().contains_key(&()));

    // Drop the original cached instance and elapse only half of the idle
    // timeout.
    drop(c0);
    time::sleep(time::Duration::from_secs(5)).await;
    assert!(handle.upgrade().is_some());
    assert!(cache.read().contains_key(&()));

    // Ensure that the handle hasn't been dropped yet and revive it to create a
    // new cached instance.
    let c1 = Cached {
        inner: (),
        // Retain the handle from the first instance.
        handle: handle.upgrade().unwrap(),
    };

    // Drop the new cache instance. Wait the remainder of the first idle timeout
    // and esnure that the handle is still retained.
    drop(c1);
    time::sleep(time::Duration::from_secs(5)).await;
    assert!(handle.upgrade().is_some());
    assert!(cache.read().contains_key(&()));

    // Wait the remainder of the second idle timeout and esnure the handle has
    // been dropped.
    time::sleep(time::Duration::from_secs(5)).await;
    assert!(handle.upgrade().is_none());
    assert!(!cache.read().contains_key(&()));
}
