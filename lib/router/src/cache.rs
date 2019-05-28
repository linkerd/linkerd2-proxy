use futures::{Async, Future, Poll, Stream};
use indexmap::IndexMap;
use std::{
    hash::Hash,
    sync::{Arc, Mutex, TryLockError, Weak},
    time::Duration,
};
use tokio_timer::{delay_queue, DelayQueue, Error, Interval};

/// A cache that is internally maintained by a `tokio_timer::DelayQueue`.
///
/// Cache access is coordinated through `vals`; this field represents the
/// current state of the cache.
///
/// Cache state is mainted by `expirations`; this field is a `DelayQueue` that
/// can be polled in the background and remove expired values.
///
/// All values in the cache will expire after a `expires` span of time.
pub struct Cache<K: Clone + Eq + Hash, V> {
    capacity: usize,
    expires: Duration,
    expirations: DelayQueue<K>,
    vals: IndexMap<K, Node<V>>,
}

/// A value that can be polled in order to eagerly remove expired cache
/// values.
///
/// This contains a weak reference to a cache so that if use of the cache is
/// dropped, it will not continue to be polled in the background.
pub struct PurgeCache<K: Clone + Eq + Hash, V> {
    cache: Weak<Mutex<Cache<K, V>>>,
    interval: Interval,
}

/// Wraps a cache value so that a lock is held on the entire cache. When the
/// access is dropped, the associated expiration time of the value in
/// `expirations` is reset.
pub struct Access<'a, K: Clone + Eq + Hash, V> {
    expires: Duration,
    expirations: &'a mut DelayQueue<K>,
    pub(crate) node: &'a mut Node<V>,
}

/// A handle to a cache value.
pub struct Node<T> {
    key: delay_queue::Key,
    pub(crate) value: T,
}

#[derive(Debug, PartialEq)]
pub struct CapacityExhausted {
    pub(crate) capacity: usize,
}

/// A handle to a cache that has capacity for at least one additional value.
pub struct Reserve<'a, K: Clone + Eq + Hash, V> {
    expirations: &'a mut DelayQueue<K>,
    expires: Duration,
    vals: &'a mut IndexMap<K, Node<V>>,
}

// ===== impl Cache =====

impl<K: Clone + Eq + Hash, V> Cache<K, V> {
    pub fn new(capacity: usize, expires: Duration) -> (Arc<Mutex<Cache<K, V>>>, PurgeCache<K, V>) {
        let cache = Self {
            capacity,
            expires,
            expirations: DelayQueue::with_capacity(capacity),
            vals: IndexMap::default(),
        };
        let cache = Arc::new(Mutex::new(cache));
        let bg_purge = PurgeCache {
            cache: Arc::downgrade(&cache),
            interval: Interval::new_interval(expires),
        };

        (cache, bg_purge)
    }

    pub fn access(&mut self, key: &K) -> Option<Access<K, V>> {
        let node = self.vals.get_mut(key)?;
        Some(Access {
            expires: self.expires,
            expirations: &mut self.expirations,
            node,
        })
    }

    pub fn poll_reserve(&mut self) -> Result<Reserve<K, V>, CapacityExhausted> {
        // If the cache capacity is zero, then we have no space to reserve a slot
        if self.capacity == 0 {
            return Err(CapacityExhausted {
                capacity: self.capacity,
            });
        }

        if self.vals.len() == self.capacity {
            match self.expirations.poll() {
                // The cache is at capacity but we are able to remove a value.
                Ok(Async::Ready(Some(entry))) => {
                    self.vals.remove(entry.get_ref());
                }

                // `Ready(None)` can only be returned when expirations is
                // empty. We know `expirations` is not empty because `vals` is
                // not empty and capacity does not equal zero.
                Ok(Async::Ready(None)) => unreachable!("cache expirations cannot be empty"),

                // The cache is at capacity and no values can be removed.
                Ok(Async::NotReady) => {
                    return Err(CapacityExhausted {
                        capacity: self.capacity,
                    });
                }

                Err(e) => panic!("Cache.expirations DelayQueue::poll must not fail: {}", e),
            }
        }

        Ok(Reserve {
            expirations: &mut self.expirations,
            expires: self.expires,
            vals: &mut self.vals,
        })
    }

    fn poll_purge(&mut self) -> Poll<(), Error> {
        while let Some(entry) = try_ready!(self.expirations.poll()) {
            self.vals.remove(entry.get_ref());
        }

        Ok(Async::Ready(()))
    }
}

// ===== impl PurgeCache =====

impl<K: Clone + Eq + Hash, V> Future for PurgeCache<K, V> {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        try_ready!(self.interval.poll().map_err(|e| {
            panic!("PurgeCache.interval Interval::poll must not fail: {}", e);
        }));

        let lock = match self.cache.upgrade() {
            Some(lock) => lock,
            // Failing to upgrade the Weak reference means the cache has been
            // dropped. We can stop trying to purge values.
            None => return Ok(Async::Ready(())),
        };
        let mut cache = match lock.try_lock() {
            // If we can lock the cache then do so
            Ok(lock) => lock,

            // If we were unable to lock the cache, panic if the cause of error
            // was a poisoned lock
            Err(TryLockError::Poisoned(e)) => panic!("lock poisoned: {:?}", e),

            // If the lock is not poisoned, it is being held by another
            // thread. Schedule this thread to be polled in the near future.
            Err(_) => {
                futures::task::current().notify();
                return Ok(Async::NotReady);
            }
        };

        cache.poll_purge().map_err(|e| {
            panic!("PurgeCache.cache Cache::poll_purge must not fail: {}", e);
        })
    }
}

// ===== impl Access =====

impl<'a, K: Clone + Eq + Hash, V> Drop for Access<'a, K, V> {
    fn drop(&mut self) {
        self.expirations.reset(&self.node.key, self.expires);
    }
}

// ===== impl Node =====

impl<T> Node<T> {
    pub fn new(key: delay_queue::Key, value: T) -> Self {
        Node { key, value }
    }
}

// ===== impl Reserve =====

impl<'a, K: Clone + Eq + Hash, V> Reserve<'a, K, V> {
    pub fn store(self, key: K, val: V) {
        let node = {
            let delay = self.expirations.insert(key.clone(), self.expires);
            Node::new(delay, val)
        };
        self.vals.insert(key, node);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future;
    use tokio::runtime::current_thread::{self, Runtime};

    #[test]
    fn reserve_and_store() {
        current_thread::run(future::lazy(|| {
            let (cache, _cache_purge) = Cache::new(2, Duration::from_millis(10));

            let mut cache = cache.lock().unwrap();

            cache.poll_reserve().expect("reserve").store(1, 2);
            assert_eq!(cache.vals.len(), 1);

            cache.poll_reserve().expect("reserve").store(2, 3);
            assert_eq!(cache.vals.len(), 2);

            assert_eq!(
                cache.poll_reserve().err(),
                Some(CapacityExhausted { capacity: 2 })
            );
            assert_eq!(cache.vals.len(), 2);

            Ok(())
        }))
    }

    #[test]
    fn store_access_value() {
        current_thread::run(future::lazy(|| {
            let (cache, _cache_purge) = Cache::new(2, Duration::from_millis(10));

            let mut cache = cache.lock().unwrap();

            assert!(cache.access(&1).is_none());
            assert!(cache.access(&2).is_none());

            cache.poll_reserve().expect("reserve").store(1, 2);
            assert!(cache.access(&1).is_some());
            assert!(cache.access(&2).is_none());

            cache.poll_reserve().expect("reserve").store(2, 3);
            assert!(cache.access(&1).is_some());
            assert!(cache.access(&2).is_some());

            assert_eq!(cache.access(&1).take().unwrap().node.value, 2);
            assert_eq!(cache.access(&2).take().unwrap().node.value, 3);

            Ok(())
        }))
    }

    #[test]
    fn reserve_does_nothing_when_capacity_exists() {
        current_thread::run(futures::future::lazy(|| {
            let (cache, _cache_purge) = Cache::new(2, Duration::from_millis(10));

            let mut cache = cache.lock().unwrap();

            cache.poll_reserve().expect("capacity").store(1, 2);
            assert_eq!(cache.vals.len(), 1);

            assert!(cache.poll_reserve().is_ok());
            assert_eq!(cache.vals.len(), 1);

            Ok(())
        }))
    }

    #[test]
    fn store_and_self_purge() {
        let mut rt = Runtime::new().unwrap();

        let (cache, _cache_purge) = rt
            .block_on(future::lazy(|| {
                Ok::<_, ()>(Cache::new(2, Duration::from_millis(10)))
            }))
            .unwrap();

        // Fill the cache, but do not spawn a background purge task
        rt.block_on(future::lazy(|| {
            let mut cache_1 = cache.lock().unwrap();

            cache_1.poll_reserve().expect("reserve").store(1, 2);
            cache_1.poll_reserve().expect("reserve").store(2, 3);
            assert_eq!(cache_1.vals.len(), 2);

            Ok::<_, ()>(())
        }))
        .unwrap();

        // Sleep for enough time that cache values should be expired
        rt.block_on(tokio_timer::sleep(Duration::from_millis(100)))
            .unwrap();

        // Force `reserve` to purge the cache
        rt.block_on(future::lazy(|| {
            let mut cache_2 = cache.lock().unwrap();

            cache_2.poll_reserve().expect("reserve").store(3, 4);
            assert_eq!(cache_2.vals.len(), 2);
            assert!(cache_2.access(&3).is_some());

            Ok::<_, ()>(())
        }))
        .unwrap()
    }

    #[test]
    fn store_and_background_purge() {
        let mut rt = Runtime::new().unwrap();

        let (cache, cache_purge) = rt
            .block_on(futures::future::lazy(|| {
                Ok::<_, ()>(Cache::new(2, Duration::from_millis(10)))
            }))
            .unwrap();

        // Spawn a background purge task on the runtime
        rt.spawn(cache_purge);

        // Fill the cache
        rt.block_on(future::lazy(|| {
            let mut cache = cache.lock().unwrap();

            cache.poll_reserve().expect("reserve").store(1, 2);
            cache.poll_reserve().expect("reserve").store(2, 3);
            assert_eq!(cache.vals.len(), 2);

            Ok::<_, ()>(())
        }))
        .unwrap();

        // Sleep for enough time that all cache values expire
        rt.block_on(tokio_timer::sleep(Duration::from_millis(100)))
            .unwrap();

        assert_eq!(cache.lock().unwrap().vals.len(), 0);
    }

    #[test]
    fn drop_access_resets_expiration() {
        let mut rt = current_thread::Runtime::new().unwrap();

        let (cache, cache_purge) = rt
            .block_on(futures::future::lazy(|| {
                Ok::<_, ()>(Cache::new(2, Duration::from_millis(10)))
            }))
            .unwrap();

        // Spawn a background purge task on the runtime
        rt.spawn(cache_purge);

        {
            let mut cache = cache.lock().unwrap();

            // Hold on to an access handle the cache
            let _access = rt
                .block_on(future::lazy(|| {
                    cache.poll_reserve().expect("reserve").store(1, 2);
                    assert!(cache.access(&1).is_some());

                    Ok::<_, ()>(cache.access(&1))
                }))
                .unwrap();

            // Sleep for enough time that the background purge would remove the value
            rt.block_on(tokio_timer::sleep(Duration::from_millis(100)))
                .unwrap();

            // Drop both the access and cache handles. Dropping the
            // access handle will reset the expiration on the value in the cache.
            // Dropping the cache handle will unlock the cache and allow a
            // background purge to occur.
        }

        // Ensure a background purge is polled so that it can expire any
        // values.
        rt.block_on(future::lazy(|| {
            tokio_timer::sleep(Duration::from_millis(1))
        }))
        .unwrap();

        rt.block_on(future::lazy(|| {
            let mut cache = cache.lock().unwrap();

            // The cache value should still be present since it was reset after
            // the value expiration. We ensured a background purge occurred but
            // that it did not purge the value.
            assert!(cache.access(&1).is_some());

            Ok::<_, ()>(())
        }))
        .unwrap()
    }
}
