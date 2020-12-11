#![deny(warnings, rust_2018_idioms)]

use linkerd2_stack::{layer, NewService};
use parking_lot::RwLock;
use std::{
    collections::{hash_map::Entry, HashMap},
    hash::Hash,
    sync::{Arc, Weak},
    task::{Context, Poll},
};
use tokio::time;
use tracing::{debug, debug_span, trace};
use tracing_futures::Instrument;

#[derive(Clone)]
pub struct Cache<T, N, S>
where
    T: Eq + Hash,
{
    inner: N,
    services: Services<T, S>,
    idle: time::Duration,
}

#[derive(Clone, Debug)]
pub struct Cached<S, T>
where
    S: Send + Sync + 'static,
    T: Eq + Hash + Send + Sync + 'static,
{
    inner: S,
    handle: Option<Handle<S, T>>,
    idle: time::Duration,
}

#[derive(Debug)]
struct Handle<S, T> {
    handle: Arc<T>,
    cache: Weak<RwLock<HashMap<T, (S, Weak<T>)>>>,
}

type Services<T, S> = Arc<RwLock<HashMap<T, (S, Weak<T>)>>>;

// === impl Cache ===

impl<T, N> Cache<T, N, N::Service>
where
    T: Eq + Hash + Send + Sync + 'static,
    N: NewService<T> + 'static,
    N::Service: Send + Sync + 'static,
{
    pub fn layer(idle: time::Duration) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(idle, inner))
    }

    pub fn new(idle: time::Duration, inner: N) -> Self {
        let services = Services::default();

        Self {
            inner,
            services,
            idle,
        }
    }
}

impl<T, N> NewService<T> for Cache<T, N, N::Service>
where
    T: Clone + Eq + Hash + Send + Sync + 'static,
    N: NewService<T>,
    N::Service: Clone + Send + Sync + 'static,
{
    type Service = Cached<N::Service, T>;

    fn new_service(&mut self, target: T) -> Cached<N::Service, T> {
        let cache = Arc::downgrade(&self.services);

        // We expect the item to be available in most cases, so initially obtain
        // only a read lock.
        if let Some((svc, weak)) = self.services.read().get(&target) {
            if let Some(handle) = weak.upgrade() {
                trace!("Using cached service");
                return Cached {
                    inner: svc.clone(),
                    idle: self.idle,
                    handle: Some(Handle { cache, handle }),
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
                        trace!("Using cached service");
                        Cached {
                            inner: svc.clone(),
                            idle: self.idle,
                            handle: Some(Handle { cache, handle }),
                        }
                    }
                    None => {
                        debug!("Replacing defunct service");
                        let inner = self.inner.new_service(target.clone());
                        let handle = Arc::new(target);
                        entry.insert((inner.clone(), Arc::downgrade(&handle)));
                        Cached {
                            inner,
                            idle: self.idle,
                            handle: Some(Handle { cache, handle }),
                        }
                    }
                }
            }
            Entry::Vacant(entry) => {
                debug!("Caching new service");
                let inner = self.inner.new_service(target.clone());
                let handle = Arc::new(target);
                entry.insert((inner.clone(), Arc::downgrade(&handle)));
                Cached {
                    inner,
                    idle: self.idle,
                    handle: Some(Handle { cache, handle }),
                }
            }
        }
    }
}

// === impl Handle ===

impl<S, T> Clone for Handle<S, T> {
    fn clone(&self) -> Self {
        Self {
            cache: self.cache.clone(),
            handle: self.handle.clone(),
        }
    }
}

// === impl Cached ===

impl<Req, S, T> tower::Service<Req> for Cached<S, T>
where
    S: tower::Service<Req> + Send + Sync + 'static,
    T: Eq + Hash + Send + Sync + 'static,
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

impl<S, T> Drop for Cached<S, T>
where
    S: Send + Sync + 'static,
    T: Eq + Hash + Send + Sync + 'static,
{
    fn drop(&mut self) {
        // If the eviction task is still running, wait for an idle timeout,
        // retaining the handle for this service. If there are no new instances
        // holding the handle after that timeout, drop the last strong reference
        // and signal the eviction task.
        trace!("Dropping cached service");
        if let Some(Handle { cache, handle }) = self.handle.take() {
            let idle = self.idle;
            tokio::spawn(
                async move {
                    trace!(?idle, "Waiting for timeout to elapse");
                    time::sleep(idle).await;
                    if let Some(cache) = cache.upgrade() {
                        // Only evict the service if we can claim the target
                        // from the handle, ensuring that there are no other
                        // strong references to the handle.
                        if let Ok(target) = Arc::try_unwrap(handle) {
                            trace!("Evicting target");
                            let mut services = cache.write();
                            if services.remove(&target).is_some() {
                                trace!("Evicted");
                            } else {
                                trace!("Service already evicted");
                            }
                        } else {
                            trace!("Handle acquired by another instance");
                        }
                    } else {
                        trace!("Cache dropped");
                    }
                }
                .instrument(debug_span!("evict")),
            );
        }
    }
}

#[cfg(test)]
#[tokio::test]
async fn cached_idle_retain() {
    use futures::prelude::*;

    let _ = tracing_subscriber::fmt::try_init();
    time::pause();

    let evict = Arc::new(Notify::new());
    let c0 = Cached {
        inner: (),
        handle: Arc::new(()),
        evict: Arc::downgrade(&evict),
        idle: time::Duration::from_secs(10),
    };
    let handle = Arc::downgrade(&c0.handle);

    // Drop the original cached instance and elapse only half of the idle
    // timeout.
    drop(c0);
    futures::select_biased! {
        _ = evict.notified().fuse() => panic!("Premature eviction"),
        _ = time::sleep(time::Duration::from_secs(5)).fuse() => {}
    }

    // Ensure that the handle hasn't been dropped yet and revive it to create a
    // new cached instance.
    assert!(handle.upgrade().is_some());
    let c1 = Cached {
        inner: (),
        handle: handle.upgrade().expect("handle must be retained"),
        evict: Arc::downgrade(&evict),
        idle: time::Duration::from_secs(10),
    };

    // Drop the new cache instance. Wait the remainder of the first idle timeout
    // and esnure that the handle is still retained.
    drop(c1);
    futures::select_biased! {
        _ = evict.notified().fuse() => panic!("Premature eviction"),
        _ = time::sleep(time::Duration::from_secs(5)).fuse() => {}
    }
    assert!(handle.upgrade().is_some());

    // Wait the remainder of the second idle timeout and esnure the handle has
    // been dropped.
    let mut sleep = time::sleep(time::Duration::from_secs(5)).fuse();

    futures::select_biased! {
        _ = evict.notified().fuse() => {}
        _ = sleep => panic!("Eviction not signaled"),
    }
    assert!(handle.upgrade().is_none());
}
