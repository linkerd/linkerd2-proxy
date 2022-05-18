#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use parking_lot::RwLock;
use std::{
    borrow::Borrow,
    collections::{
        hash_map::{Entry, RandomState},
        HashMap,
    },
    hash::{BuildHasher, BuildHasherDefault, Hash},
    ops::{Deref, DerefMut},
    sync::{Arc, Weak},
    task::{Context, Poll},
};
use tokio::{sync::Notify, time};
use tracing::{debug, instrument, trace};
mod new_service;

pub use new_service::NewCachedService;

#[derive(Clone)]
pub struct Cache<K, V, S = RandomState>
where
    K: Eq + Hash,
{
    inner: Arc<Inner<K, V, S>>,
    idle: time::Duration,
}

#[derive(Clone, Debug)]
pub struct Cached<V> {
    inner: V,
    // Notifies entry's eviction task that a drop has occurred.
    handle: Option<Arc<Notify>>,
}

type Inner<K, V, S> = RwLock<HashMap<K, (V, Weak<Notify>), S>>;

// === impl Cache ===

impl<K, V> Cache<K, V>
where
    K: Clone + std::fmt::Debug + Eq + Hash + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    pub fn new(idle: time::Duration) -> Self {
        Self::with_hasher(idle, Default::default())
    }
}

impl<K, V, S> Cache<K, V, BuildHasherDefault<S>>
where
    K: Clone + std::fmt::Debug + Eq + Hash + Send + Sync + 'static,
    V: Send + Sync + 'static,
    BuildHasherDefault<S>: BuildHasher + Send + Sync + 'static,
{
    pub fn from_iter(idle: time::Duration, iter: impl IntoIterator<Item = (K, V)>) -> Self
    where
        S: Default,
        V: Clone,
    {
        let iter = iter.into_iter();
        let (lower_bound, _) = iter.size_hint();
        let inner = Arc::new(RwLock::new(HashMap::with_capacity_and_hasher(
            lower_bound,
            BuildHasherDefault::default(),
        )));
        let this = Self { inner, idle };
        // XXX(eliza): having to go through `get_or_insert_with` rather than
        // building a map directly is a shame, but `spawn_idle` requires a ref
        // the map, so...
        for (key, value) in iter {
            this.get_or_insert_with(key, move |_| value);
        }
        this
    }
}

impl<K, V, S> Cache<K, V, S>
where
    K: Clone + std::fmt::Debug + Eq + Hash + Send + Sync + 'static,
    V: Send + Sync + 'static,
    S: BuildHasher + Send + Sync + 'static,
{
    pub fn with_hasher(idle: time::Duration, hasher: S) -> Self {
        let inner = Arc::new(RwLock::new(HashMap::with_hasher(hasher)));
        Self { inner, idle }
    }

    pub fn get<Q: ?Sized>(&self, key: &Q) -> Option<Cached<V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
        V: Clone,
    {
        let lock = self.inner.read();
        let (value, weak) = lock.get(key)?;
        let handle = weak.upgrade()?;

        trace!("Using cached value");
        Some(Cached {
            inner: value.clone(),
            handle: Some(handle),
        })
    }

    pub fn get_or_insert_with(&self, key: K, f: impl FnOnce(K) -> V) -> Cached<V>
    where
        V: Clone,
    {
        // We expect the item to be available in most cases, so initially obtain
        // only a read lock.
        if let Some(val) = self.get(&key) {
            return val;
        }

        // Otherwise, obtain a write lock to insert a new value.
        match self.inner.write().entry(key.clone()) {
            Entry::Occupied(ref mut entry) => {
                let (value, weak) = entry.get();
                // Another thread raced us to create a value for this target.
                // Try to use it.
                match weak.upgrade() {
                    Some(handle) => {
                        trace!(?key, "Using cached value");
                        Cached {
                            inner: value.clone(),
                            handle: Some(handle),
                        }
                    }
                    None => {
                        debug!(?key, "Replacing defunct value");
                        let handle = self.spawn_idle(key.clone());
                        let inner = f(key);
                        entry.insert((inner.clone(), Arc::downgrade(&handle)));
                        Cached {
                            inner,
                            handle: Some(handle),
                        }
                    }
                }
            }
            Entry::Vacant(entry) => {
                debug!(?key, "Caching new value");
                let handle = self.spawn_idle(key.clone());
                let inner = f(key);
                entry.insert((inner.clone(), Arc::downgrade(&handle)));
                Cached {
                    inner,
                    handle: Some(handle),
                }
            }
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
    async fn evict(
        key: K,
        idle: time::Duration,
        mut reset: Arc<Notify>,
        cache: Weak<Inner<K, V, S>>,
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

impl<V> Cached<V> {
    /// Returns a new `Cached` handle wrapping the provided value, but *not*
    /// associated with a cache.
    ///
    /// This is intended for use in cases where most values returned by a
    /// function are stored in a cache, but some may instead be fixed, uncached
    /// values.
    ///
    /// The uncached `Cached` instance will never be evicted, since it didn't
    /// come from a cache.
    pub fn uncached(inner: V) -> Self {
        Self {
            inner,
            handle: None,
        }
    }
}

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

impl<V> Deref for Cached<V> {
    type Target = V;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<V> DerefMut for Cached<V> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
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
    let c0 = Cached {
        inner: (),
        handle: Some(handle),
    };

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
