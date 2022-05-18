#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use parking_lot::{
    MappedRwLockReadGuard, MappedRwLockWriteGuard, RwLock, RwLockReadGuard, RwLockWriteGuard,
};
use std::{
    borrow::Borrow,
    collections::{HashMap},
    hash::Hash,
    ops::Deref,
    sync::{Arc, Weak},
    task::{Context, Poll},
};
use tokio::{sync::Notify, time};
use tracing::{debug, instrument, trace};
mod new_service;

pub use new_service::NewCachedService;

#[derive(Clone)]
pub struct Cache<K, V>
where
    K: Eq + Hash,
{
    inner: Arc<Inner<K, V>>,
    idle: time::Duration,
}

#[derive(Clone, Debug)]
pub struct Cached<V> {
    inner: V,
    // Notifies entry's eviction task that a drop has occurred.
    handle: Option<Arc<Notify>>,
}

type Inner<K, V> = RwLock<HashMap<K, (V, Weak<Notify>)>>;

pub enum Ref<'a, V> {
    Read(MappedRwLockReadGuard<'a, V>),
    Inserted(MappedRwLockWriteGuard<'a, (V, Weak<Notify>)>),
}

// === impl Cache ===

impl<K, V> Cache<K, V>
where
    K: Clone + std::fmt::Debug + Eq + Hash + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    pub fn new(idle: time::Duration) -> Self {
        let inner = Arc::new(Inner::default());
        Self { inner, idle }
    }

    pub fn get<'a, Q: ?Sized>(&'a self, key: &Q) -> Option<Cached<Ref<'a, V>>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let mut handle = None;
        let value = RwLockReadGuard::try_map(self.inner.read(), |inner| {
            let (value, weak) = inner.get(&key)?;
            handle = Some(weak.upgrade()?);
            Some(value)
        })
        .ok()?;

        trace!("Using cached value");
        Some(Cached {
            inner: Ref::Read(value),
            handle,
        })
    }

    pub fn get_or_insert_with(&self, key: K, f: impl FnOnce(K) -> V) -> Cached<Ref<'_, V>> {
        // We expect the item to be available in most cases, so initially obtain
        // only a read lock.
        if let Some(val) = self.get(&key) {
            return val;
        }

        let mut f = Some(f);

        // Otherwise, obtain a write lock to insert a new value.
        let mut handle = None;
        let value = RwLockWriteGuard::map(self.inner.write(), |inner| {
            inner
                .entry(key.clone())
                .and_modify(|(value, weak)| {
                    // Another thread raced us to create a value for this target.
                    // Try to use it.
                    match weak.upgrade() {
                        Some(h) => {
                            trace!(?key, "Using cached value");
                            handle = Some(h);
                        }
                        None => {
                            debug!(?key, "Replacing defunct value");
                            let new_handle = self.spawn_idle(key.clone());
                            let f = f.take().expect("function is only called a single time");
                            *value = f(key.clone());
                            *weak = Arc::downgrade(&new_handle);
                            handle = Some(new_handle);
                        }
                    }
                })
                .or_insert_with(|| {
                    debug!(?key, "Caching new value");
                    let new_handle = self.spawn_idle(key.clone());
                    let f = f.take().expect("function is only called a single time");
                    let inner = f(key.clone());
                    let weak = Arc::downgrade(&new_handle);
                    handle = Some(new_handle);
                    (inner, weak)
                })
        });

        Cached {
            handle,
            inner: Ref::Inserted(value),
        }
    }

    fn spawn_idle(&self, key: K) -> Arc<Notify> {
        // Spawn a background task that holds the handle. Every time the handle
        // is notified, it resets the idle timeout. Every time teh idle timeout
        // expires, the handle is checked and the service is dropped if there
        // are no active handles.
        let handle = Arc::new(Notify::new());
        tokio::spawn(Self::evict(
            key,
            self.idle,
            handle.clone(),
            Arc::downgrade(&self.inner),
        ));
        handle
    }

    #[instrument(level = "debug", skip(idle, reset, cache))]
    async fn evict(key: K, idle: time::Duration, mut reset: Arc<Notify>, cache: Weak<Inner<K, V>>) {
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
                            let removed = cache.write().remove(&key).is_some();
                            debug_assert!(removed, "Cache item must exist: {:?}", key);
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

impl<V> Deref for Cached<Ref<'_, V>> {
    type Target = V;
    fn deref(&self) -> &Self::Target {
        match self.inner {
            Ref::Read(ref guard) => guard.deref(),
            Ref::Inserted(ref guard) => &guard.deref().0,
        }
    }
}

impl<V: Clone> Cached<Ref<'_, V>> {
    pub fn into_owned(mut self) -> Cached<V> {
        let inner = V::clone(Deref::deref(&self));
        Cached {
            inner,
            handle: self.handle.take(),
        }
    }
}

impl<V> Drop for Cached<V> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.notify_one();
        }
    }
}


#[cfg(test)]
#[tokio::test(flavor = "current_thread")]
async fn test_idle_retain() {
    time::pause();

    let idle = time::Duration::from_secs(10);
    let cache = Cache::new(idle);

    let handle = cache.spawn_idle(());
    let weak = Arc::downgrade(&handle);
    cache.inner.write().insert((), ((), weak.clone()));
    let c0 = Cached { inner: (), handle: Some(handle) };


    // Let an idle timeout elapse and ensured the held service has not been
    // evicted.
    time::sleep(idle * 2).await;
    assert!(weak.upgrade().is_some());
    assert!(cache.inner.read().contains_key(&()));

    // Drop the original cached instance and elapse only half of the idle
    // timeout.
    drop(c0);
    time::sleep(time::Duration::from_secs(5)).await;
    assert!(weak.upgrade().is_some());
    assert!(cache.inner.read().contains_key(&()));

    // Ensure that the handle hasn't been dropped yet and revive it to create a
    // new cached instance.
    let c1 = Cached {
        inner: (),
        // Retain the handle from the first instance.
        handle: Some(weak.upgrade().unwrap()),
    };

    // Drop the new cache instance. Wait the remainder of the first idle timeout
    // and esnure that the handle is still retained.
    drop(c1);
    time::sleep(time::Duration::from_secs(5)).await;
    assert!(weak.upgrade().is_some());
    assert!(cache.inner.read().contains_key(&()));

    // Wait the remainder of the second idle timeout and esnure the handle has
    // been dropped.
    time::sleep(time::Duration::from_secs(5)).await;
    assert!(weak.upgrade().is_none());
    assert!(!cache.inner.read().contains_key(&()));
}
