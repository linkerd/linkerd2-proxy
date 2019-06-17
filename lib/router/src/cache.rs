use std::{hash::Hash, time::Duration};

use futures::{Async, Future, Poll, Stream};
use indexmap::IndexMap;
use never::Never;
use tokio::sync::lock::Lock;
use tokio_timer::{delay_queue, DelayQueue, Error, Interval};

/// An LRU cache that can eagerly remove values in a background task.
///
/// Assumptions:
///     - `get` is common
///     - `poll_insert` is less common
///     - Values should have an `expires` span of time greater than 0
///
/// Complexity:
///     - `get` in **O(1)** time (amortized average)
///     - `insert` in **O(1)** time (average)
///     - `poll_insert` in **O(1)** time (average)
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
    /// Elements are keys into `values`. As elements become Ready, we can
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

    pub fn get(&mut self, key: &K) -> Option<V> {
        if let Some(node) = self.values.get_mut(key) {
            self.expirations.reset(&node.dq_key, self.expires);
            return Some(node.value.clone());
        }

        None
    }

    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        let node = {
            let dq_key = self.expirations.insert(key.clone(), self.expires);
            Node { dq_key, value }
        };

        self.values.insert(key, node).map(|n| n.value)
    }

    pub fn poll_insert(&mut self) -> Poll<(), Never> {
        // When checking capacity, only try to remove values if the cache is
        // at capacity
        if self.values.len() == self.capacity {
            match self
                .expirations
                .poll()
                .expect("delay_queue::poll must not fail")
            {
                // The cache is at capacity, but we are able to remove a value
                Async::Ready(Some(expired)) => {
                    self.values.remove(expired.get_ref());
                }

                // `Ready(None)` can only be returned when `expirations` is
                // empty; we know `expirations` is not empty because `values`
                // is not empty and capacity does not equal zero.
                Async::Ready(None) => unreachable!("cache expirations cannot be empty"),

                // The cache is at capacity and no values can be removed
                Async::NotReady => {
                    return Ok(Async::NotReady);
                }
            }
        }

        Ok(Async::Ready(()))
    }

    fn poll_purge(&mut self) -> Poll<(), Error> {
        while let Some(expired) = try_ready!(self.expirations.poll()) {
            self.values.remove(expired.get_ref());
        }

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
        try_ready!(self.interval.poll().map_err(|e| {
            panic!("Interval::poll must not fail: {}", e);
        }));

        let mut cache = try_ready!(Ok(self.cache.poll_lock()));

        cache.poll_purge().map_err(|e| {
            panic!("Cache::poll_purge must not fail: {}", e);
        })
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

            {
                match cache.poll_insert().unwrap() {
                    Async::Ready(()) => (),
                    _ => panic!("cache should be Ready to reserve"),
                };
                cache.insert(1, 2);
            }
            assert_eq!(cache.values.len(), 1);

            {
                match cache.poll_insert().unwrap() {
                    Async::Ready(()) => (),
                    _ => panic!("cache should be Ready to reserve"),
                };
                cache.insert(2, 3);
            }
            assert_eq!(cache.values.len(), 2);

            match cache.poll_insert().unwrap() {
                Async::NotReady => (),
                _ => panic!("cache should not be Ready to reserve"),
            }
            assert_eq!(cache.values.len(), 2);

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

            assert!(cache.get(&1).is_none());
            assert!(cache.get(&2).is_none());

            {
                match cache.poll_insert().unwrap() {
                    Async::Ready(()) => (),
                    _ => panic!("cache should be Ready to reserve"),
                };
                cache.insert(1, 2);
            }
            assert!(cache.get(&1).is_some());
            assert!(cache.get(&2).is_none());

            {
                match cache.poll_insert().unwrap() {
                    Async::Ready(()) => (),
                    _ => panic!("cache should be Ready to reserve"),
                };
                cache.insert(2, 3);
            }
            assert!(cache.get(&1).is_some());
            assert!(cache.get(&2).is_some());

            assert_eq!(cache.get(&1).take().unwrap(), 2);
            assert_eq!(cache.get(&2).take().unwrap(), 3);

            Ok::<_, ()>(())
        }))
    }

    #[test]
    fn poll_insert_does_nothing_when_capacity_exists() {
        current_thread::run(futures::future::lazy(|| {
            let (mut cache, _cache_purge) = Cache::new(2, Duration::from_millis(10));

            let mut cache = match cache.poll_lock() {
                Async::Ready(acquired) => acquired,
                _ => panic!("cache lock should be Ready"),
            };

            {
                match cache.poll_insert().unwrap() {
                    Async::Ready(()) => (),
                    _ => panic!("cache should be Ready to reserve"),
                };
                cache.insert(1, 2);
            }
            assert_eq!(cache.values.len(), 1);

            {
                match cache.poll_insert().unwrap() {
                    Async::Ready(()) => (),
                    _ => panic!("cache should be Ready to reserve"),
                };
            }
            assert_eq!(cache.values.len(), 1);

            Ok::<_, ()>(())
        }))
    }

    #[test]
    fn insert_and_self_purge() {
        let mut rt = Runtime::new().unwrap();

        let (mut cache, _cache_purge) = rt
            .block_on(future::lazy(|| {
                Ok::<_, ()>(Cache::new(2, Duration::from_millis(10)))
            }))
            .unwrap();

        // Fill the cache, but do not spawn a background purge task
        rt.block_on(future::lazy(|| {
            let mut cache_1 = match cache.poll_lock() {
                Async::Ready(acquired) => acquired,
                _ => panic!("cache lock should be Ready"),
            };

            {
                match cache_1.poll_insert().unwrap() {
                    Async::Ready(()) => (),
                    _ => panic!("cache should be Ready to reserve"),
                };
                cache_1.insert(1, 2);
            }
            {
                match cache_1.poll_insert().unwrap() {
                    Async::Ready(()) => (),
                    _ => panic!("cache should be Ready to reserve"),
                };
                cache_1.insert(2, 3);
            }
            assert_eq!(cache_1.values.len(), 2);

            Ok::<_, ()>(())
        }))
        .unwrap();

        // Sleep for enough time that cache values should be expired
        rt.block_on(tokio_timer::sleep(Duration::from_millis(100)))
            .unwrap();

        // Force `reserve` to purge the cache
        rt.block_on(future::lazy(|| {
            let mut cache_2 = match cache.poll_lock() {
                Async::Ready(acquired) => acquired,
                _ => panic!("cache lock should be Ready"),
            };

            {
                match cache_2.poll_insert().unwrap() {
                    Async::Ready(()) => (),
                    _ => panic!("cache should be Ready to reserve"),
                };
                cache_2.insert(3, 4);
            }
            assert_eq!(cache_2.values.len(), 2);
            assert!(cache_2.get(&3).is_some());

            Ok::<_, ()>(())
        }))
        .unwrap()
    }

    #[test]
    fn isnert_and_background_purge() {
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

            {
                match cache.poll_insert().unwrap() {
                    Async::Ready(()) => (),
                    _ => panic!("cache should be Ready to reserve"),
                };
                cache.insert(1, 2);
            }
            {
                match cache.poll_insert().unwrap() {
                    Async::Ready(()) => (),
                    _ => panic!("cache should be Ready to reserve"),
                };
                cache.insert(2, 3);
            }
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

            match cache.poll_insert().unwrap() {
                Async::Ready(()) => (),
                _ => panic!("cache should be Ready to reserve"),
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
            assert!(cache.get(&1).is_some());

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
            assert!(cache.get(&1).is_some());

            Ok::<_, ()>(())
        }))
        .unwrap();
    }
}
