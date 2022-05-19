#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use parking_lot::RwLock;
use std::{
    borrow::Borrow,
    collections::{
        hash_map::{Entry, RandomState},
        HashMap,
    },
    fmt,
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
    /// The amount of time after which an entry is considered idle and may be
    /// evicted.
    idle: time::Duration,

    inner: Arc<InnerMap<K, V, S>>,
}

/// A handle that that holds the referenced value in the cache. When dropped,
/// the cache may drop the value from the cache after an idle timeout.
#[derive(Clone, Debug)]
pub struct Cached<V> {
    inner: V,

    // Notifies entry's eviction task that a drop has occurred. If no handle is
    // set, then the entry is permanent and will not be evicted.
    handle: Option<Arc<Notify>>,
}

#[derive(Debug)]
struct CacheEntry<V> {
    value: V,
    /// A handle to wake the expiration task.
    ///
    /// If this is unset, the entry is permanent and will not be evicted.
    handle: Option<Weak<Notify>>,
}

/// A locked cache map holding values and an optional handle. When the handle is
/// unset, the entry is 'permanent' and will never be evicted from the map. When
/// a handle is set, it is used to notify the eviction task that an entry has
/// been dropped.
type InnerMap<K, V, S> = RwLock<HashMap<K, CacheEntry<V>, S>>;

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
    /// Creates a new cache with an initial capacity.
    pub fn with_capacity(idle: time::Duration, capacity: usize) -> Self {
        Self {
            idle,
            inner: Arc::new(RwLock::new(HashMap::with_capacity_and_hasher(
                capacity,
                BuildHasherDefault::default(),
            ))),
        }
    }

    /// Creates a new cache with a set of permanent entries.
    pub fn with_permanent_from_iter(
        idle: time::Duration,
        iter: impl IntoIterator<Item = (K, V)>,
    ) -> Self
    where
        S: Default,
        V: Clone,
    {
        let entries = iter
            .into_iter()
            .map(|(k, v)| (k, CacheEntry::permanent(v)))
            .collect();
        let inner = Arc::new(RwLock::new(entries));
        Self { inner, idle }
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
        Q: Hash + Eq + fmt::Debug,
        V: Clone,
    {
        let cache = self.inner.read();
        let cache_entry = cache.get(&key)?;
        let cached = cache_entry.cached();

        trace!(
            ?key,
            entry.is_permanent = cache_entry.is_permanent(),
            "Using cached value"
        );
        Some(cached)
    }

    pub fn get_or_insert_with(&self, key: K, f: impl FnOnce(&K) -> V) -> Cached<V>
    where
        V: Clone,
    {
        // We expect the item to be available in most cases, so initially obtain
        // only a read lock.
        if let Some(val) = self.get(&key) {
            return val;
        }

        // Otherwise, obtain a write lock to insert a new value.
        let mut cache = self.inner.write();
        match cache.entry(key) {
            Entry::Vacant(entry) => {
                debug!(key = ?entry.key(), "Caching new value");
                let inner = f(entry.key());
                let handle = self.spawn_idle(entry.key().clone());
                entry.insert(CacheEntry {
                    value: inner.clone(),
                    handle: Some(Arc::downgrade(&handle)),
                });
                Cached {
                    inner,
                    handle: Some(handle),
                }
            }

            Entry::Occupied(entry) => {
                // Another thread raced us to create a value for this target.
                trace!(key = ?entry.key(), "Using cached value");
                entry.get().clone().cached()
            }
        }
    }

    /// Adds or overwrites a value in the cache that will never be evicted from
    /// the cache.
    pub fn insert_permanent(&self, key: K, val: V) -> Option<V> {
        match self.inner.write().entry(key) {
            Entry::Vacant(entry) => {
                debug!(key = ?entry.key(), "Permanently caching new value");
                entry.insert(CacheEntry::permanent(val));
                None
            }
            Entry::Occupied(ref mut entry) => {
                debug!(key = ?entry.key(), "Updating permanently cached value");
                let prior_entry = entry.insert(CacheEntry::permanent(val));
                Some(prior_entry.value)
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
        cache: Weak<InnerMap<K, V, S>>,
    ) {
        // Wait for the handle to be notified before starting to track idleness.
        reset.notified().await;
        debug!("Awaiting idleness");

        loop {
            // Wait until the idle timeout expires to check to see if the entry
            // should be evicted from the cache.
            let cache = tokio::select! {
                biased;

                // If the reset was notified, restart the timer (and skip
                // checking the cache).
                _ = reset.notified() => {
                    trace!("Reset");
                    continue;
                }

                // If the timeout expires, try to clear the key from the cache...
                _ = time::sleep(idle) => match cache.upgrade() {
                    Some(c) => c,
                    None => {
                        trace!("Cache already dropped");
                        return;
                    }
                },
            };

            // Lock the cache before checking the handle.
            //
            // Otherwise, if we consume the reset handle first, it's possible
            // for another task to update the cache entry before we lock the
            // cache.
            let mut cache = cache.write();

            // Try to consume the reset handle to ensure no other tasks are
            // holding a clone
            if let Err(r) = Arc::try_unwrap(reset) {
                // The handle is still being held elsewhere, So wait for another
                // idle timeout to check again.
                reset = r;
                continue;
            }

            // If this was the last handle, attempt to clear the key from the
            // cache (unless it was replaced by a permanent value). There should
            // be at most one task per key, so we expect the key to be in the
            // cached.
            let entry = cache.entry(key);
            debug_assert!(
                matches!(entry, Entry::Occupied(_)),
                "Cache item must exist: {:?}",
                entry.key()
            );
            if let Entry::Occupied(entry) = entry {
                if entry.get().is_permanent() {
                    // The key was updated with a permanent value that cannot be
                    // evicted.
                    debug!(key = ?entry.key(), "Cache entry was replaced by permanent value");
                    return;
                }

                debug!(key = ?entry.key(), "Dropping cache entry");
                entry.remove();
            }

            // The entry no longer exists in the cache.
            return;
        }
    }
}

impl<V> CacheEntry<V> {
    fn permanent(value: V) -> Self {
        Self {
            value,
            handle: None,
        }
    }

    fn cached(&self) -> Cached<V>
    where
        V: Clone,
    {
        let handle = self.handle.as_ref().map(|handle| {
            handle
                .upgrade()
                .expect("handles must be held as long as the entry is in the cache")
        });
        Cached {
            inner: self.value.clone(),
            handle,
        }
    }

    fn is_permanent(&self) -> bool {
        self.handle.is_none()
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
    cache.inner.write().insert((), ((), Some(weak.clone())));
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
