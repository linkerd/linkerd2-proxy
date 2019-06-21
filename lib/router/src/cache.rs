use std::{hash::Hash, time::Duration};

use futures::{try_ready, Async, Future, Poll, Stream};
use indexmap::IndexMap;
use log::trace;
use tokio::sync::lock::Lock;
use tokio_timer::{delay_queue, DelayQueue, Error, Interval};

/// An LRU cache that can eagerly remove values in a background task.
///
/// Assumptions:
/// - `access` is common
/// - `insert` is less common
/// - Values should have an `expires` span of time greater than 0
///
/// Complexity:
/// - `access` in **O(1)** time (amortized average)
/// - `insert` in **O(1)** time (average)
///
/// The underlying data structure of Cache is a [`DelayQueue`]. This allows
/// the background task to remove values by polling for elements that have
/// reached their specified deadline. Elements are retrieved from the queue
/// via [`Stream::poll`], and that is what [`poll_purge`] ultimately uses.
///
/// [`DelayQueue`]: https://docs.rs/tokio/0.1.19/tokio/timer/struct.DelayQueue.html
/// [`Stream::poll`]: https://docs.rs/tokio/0.1.19/tokio/timer/struct.DelayQueue.html#impl-Stream
/// [`poll_purge`]: #method.poll_purge
pub struct Cache<K, V>
where
    K: Clone + Eq + Hash,
{
    capacity: usize,
    expires: Duration,
    /// A queue of keys into `values` that become ready when the corresponding
    /// cache entry expires. As elements become ready, we can
    /// remove the key and corresponding value from the cache.
    expirations: DelayQueue<K>,
    /// Cache access is coordinated through `values`. This field represents
    /// the current state of the cache.
    values: IndexMap<K, Node<V>>,
}

/// A background future that eagerly removes expired cache values.
///
/// If the cache is dropped, this future will complete.
pub struct PurgeCache<K, V>
where
    K: Clone + Eq + Hash,
{
    cache: Lock<Cache<K, V>>,
    interval: Interval,
}

/// A handle to a cache value.
struct Node<T> {
    dq_key: delay_queue::Key,
    value: T,
}

// ===== impl Cache =====

impl<K, V> Cache<K, V>
where
    K: Clone + Eq + Hash,
    V: Clone,
{
    pub fn new(capacity: usize, expires: Duration) -> (Lock<Cache<K, V>>, PurgeCache<K, V>) {
        assert!(capacity != 0);
        let cache = Self {
            capacity,
            expires,
            expirations: DelayQueue::with_capacity(capacity),
            values: IndexMap::default(),
        };
        let cache = Lock::new(cache);
        let bg_purge = PurgeCache {
            cache: cache.clone(),
            interval: Interval::new_interval(expires),
        };

        (cache, bg_purge)
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn can_insert(&self) -> bool {
        self.values.len() < self.capacity
    }

    /// Attempts to access an item by key.
    ///
    /// If a value is returned, this key will not be considered for eviction
    /// for another `expires` span of time.
    pub fn access(&mut self, key: &K) -> Option<V> {
        if let Some(node) = self.values.get_mut(key) {
            self.expirations.reset(&node.dq_key, self.expires);
            trace!("reset expiration for cache value associated with key");

            return Some(node.value.clone());
        }

        None
    }

    /// Attempts to insert an item by key.
    ///
    /// If a value is returned, this key has been set to expire after an
    /// `expires` span of time.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        let node = {
            trace!("inserting value into expirations");
            let dq_key = self.expirations.insert(key.clone(), self.expires);
            Node { dq_key, value }
        };

        trace!("inserting node into values");
        self.values.insert(key, node).map(|n| n.value)
    }

    /// Evict expired values from the cache.
    ///
    /// Polls the underlying `DelayQueue`. When elements are returned from the
    /// queue, remove the associated key from `values`.
    fn poll_purge(&mut self) -> Poll<(), Error> {
        while let Some(expired) = try_ready!(self.expirations.poll()) {
            trace!("expired an entry from expirations");
            self.values.remove(expired.get_ref());
            trace!("removed an entry from values!");
        }

        trace!("cache expirations polling is ready");
        Ok(Async::Ready(()))
    }
}

// ===== impl PurgeCache =====

impl<K, V> Future for PurgeCache<K, V>
where
    K: Clone + Eq + Hash,
    V: Clone,
{
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            try_ready!(self.interval.poll().map_err(|e| {
                panic!("Interval::poll must not fail: {}", e);
            }));
            trace!("purge task interval reached");

            let mut cache = try_ready!(Ok(self.cache.poll_lock()));
            trace!("purge task locked the router cache");

            // If poll_purge is not ready, we cannot expire any values and
            // wait for the next interval. If poll_purge is ready, we have
            // expired all values and wait for the next interval
            trace!("purge task is polling for evictions...");
            cache.poll_purge().expect("Cache::poll_purge must not fail");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future;
    use tokio::runtime::current_thread::{self, Runtime};

    #[test]
    fn check_capacity_and_insert() {
        current_thread::run(future::lazy(|| {
            let (mut cache, _cache_purge) = Cache::new(2, Duration::from_millis(10));

            let mut cache = match cache.poll_lock() {
                Async::Ready(acquired) => acquired,
                _ => panic!("cache lock should be Ready"),
            };

            cache.insert(1, 2);
            assert_eq!(cache.values.len(), 1);

            cache.insert(2, 3);
            assert_eq!(cache.values.len(), 2);
            assert!(!cache.can_insert());

            Ok::<_, ()>(())
        }))
    }

    #[test]
    fn insert_and_access_value() {
        current_thread::run(future::lazy(|| {
            let (mut cache, _cache_purge) = Cache::new(2, Duration::from_millis(10));

            let mut cache = match cache.poll_lock() {
                Async::Ready(acquired) => acquired,
                _ => panic!("cache lock should be Ready"),
            };

            assert!(cache.access(&1).is_none());
            assert!(cache.access(&2).is_none());

            cache.insert(1, 2);
            assert!(cache.access(&1).is_some());
            assert!(cache.access(&2).is_none());

            cache.insert(2, 3);
            assert!(cache.access(&1).is_some());
            assert!(cache.access(&2).is_some());

            assert_eq!(cache.access(&1).take().unwrap(), 2);
            assert_eq!(cache.access(&2).take().unwrap(), 3);

            Ok::<_, ()>(())
        }))
    }

    #[test]
    fn insert_and_background_purge() {
        let mut rt = Runtime::new().unwrap();

        let (mut cache, cache_purge) = rt
            .block_on(futures::future::lazy(|| {
                Ok::<_, ()>(Cache::new(2, Duration::from_millis(10)))
            }))
            .unwrap();

        // Spawn a background purge task on the runtime
        rt.spawn(cache_purge);

        // Fill the cache
        rt.block_on(future::lazy(|| {
            let mut cache = match cache.poll_lock() {
                Async::Ready(acquired) => acquired,
                _ => panic!("cache lock should be Ready"),
            };

            cache.insert(1, 2);
            cache.insert(2, 3);
            assert_eq!(cache.values.len(), 2);

            Ok::<_, ()>(())
        }))
        .unwrap();

        // Sleep for enough time that all cache values expire
        rt.block_on(tokio_timer::sleep(Duration::from_millis(100)))
            .unwrap();

        let cache = match cache.poll_lock() {
            Async::Ready(acquired) => acquired,
            _ => panic!("cache lock should be Ready"),
        };
        assert_eq!(cache.values.len(), 0);
    }

    #[test]
    fn access_resets_expiration() {
        let mut rt = Runtime::new().unwrap();

        let (mut cache, cache_purge) = rt
            .block_on(future::lazy(|| {
                Ok::<_, ()>(Cache::new(2, Duration::from_millis(100)))
            }))
            .unwrap();

        // Spawn a background purge task on the runtime
        rt.spawn(cache_purge);

        // Insert into the cache
        rt.block_on(future::lazy(|| {
            let mut cache = match cache.poll_lock() {
                Async::Ready(acquired) => acquired,
                _ => panic!("cache lock should be Ready"),
            };

            cache.insert(1, 2);
            assert_eq!(cache.values.len(), 1);

            Ok::<_, ()>(())
        }))
        .unwrap();

        // Sleep for at least half of the expiration time
        rt.block_on(tokio_timer::sleep(Duration::from_millis(60)))
            .unwrap();

        // Access the value that was inserted
        rt.block_on(future::lazy(|| {
            let mut cache = match cache.poll_lock() {
                Async::Ready(acquired) => acquired,
                _ => panic!("cache lock should be Ready"),
            };
            assert!(cache.access(&1).is_some());

            Ok::<_, ()>(())
        }))
        .unwrap();

        // Sleep for at least half of the expiration time
        rt.block_on(tokio_timer::sleep(Duration::from_millis(60)))
            .unwrap();

        // If the access reset the value's expiration, it should still be
        // retrievable
        rt.block_on(future::lazy(|| {
            let mut cache = match cache.poll_lock() {
                Async::Ready(acquired) => acquired,
                _ => panic!("cache lock should be Ready"),
            };
            assert!(cache.access(&1).is_some());

            Ok::<_, ()>(())
        }))
        .unwrap();
    }
}
