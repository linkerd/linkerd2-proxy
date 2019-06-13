use futures::{Async, Future, Poll, Stream};
use indexmap::IndexMap;
use std::{hash::Hash, time::Duration};
use tokio::sync::lock::Lock;
use tokio_timer::{DelayQueue, Error, Interval};

/// A cache that is internally maintained by a `tokio_timer::DelayQueue`.
///
/// All values in the cache will expire after a `expires` span of time.
pub struct Cache<K, V>
where
    K: Clone + Eq + Hash,
{
    capacity: usize,
    expires: Duration,
    /// Elements are keys into `values`. As elements become Ready, we can remove
    /// the key and corresponding value from the cache.
    expirations: DelayQueue<K>,
    /// Cache access is coordinated through `values`. This field represents the
    /// current state of the cache.
    values: IndexMap<K, V>,
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

#[derive(Debug, PartialEq)]
pub struct CapacityExhausted {
    pub(crate) capacity: usize,
}

/// A handle to a cache that has capacity for at least one additional value.
pub struct Reserve<'a, K, V>
where
    K: Clone + Eq + Hash,
{
    expirations: &'a mut DelayQueue<K>,
    expires: Duration,
    values: &'a mut IndexMap<K, V>,
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

    pub fn access(&mut self, key: &K) -> Option<V> {
        let value = self.values.get_mut(key)?;
        Some(value.clone())
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn reserve(&mut self) -> Poll<Reserve<K, V>, ()> {
        if self.values.len() == self.capacity {
            match self.expirations.poll() {
                // The cache is at capacity but we are able to remove a value.
                Ok(Async::Ready(Some(entry))) => {
                    self.values.remove(entry.get_ref());
                }

                // `Ready(None)` can only be returned when expirations is
                // empty. We know `expirations` is not empty because `values` is
                // not empty and capacity does not equal zero.
                Ok(Async::Ready(None)) => unreachable!("cache expirations cannot be empty"),

                // The cache is at capacity and no values can be removed.
                Ok(Async::NotReady) => {
                    return Ok(Async::NotReady);
                }

                Err(e) => panic!("Cache.expirations DelayQueue::poll must not fail: {}", e),
            }
        }

        Ok(Async::Ready(Reserve {
            expirations: &mut self.expirations,
            expires: self.expires,
            values: &mut self.values,
        }))
    }

    fn poll_purge(&mut self) -> Poll<(), Error> {
        while let Some(entry) = try_ready!(self.expirations.poll()) {
            self.values.remove(entry.get_ref());
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
            panic!("PurgeCache.interval Interval::poll must not fail: {}", e);
        }));

        let mut acquired = try_ready!(Ok(self.cache.poll_lock()));

        acquired.poll_purge().map_err(|e| {
            panic!("PurgeCache.cache Cache::poll_purge must not fail: {}", e);
        })
    }
}

// ===== impl Access =====

// impl<'a, K, V> Drop for Access<'a, K, V>
// where
//     K: Clone + Eq + Hash,
// {
//     fn drop(&mut self) {
//         self.expirations.reset(&self.node.key, self.expires);
//     }
// }

// ===== impl Reserve =====

impl<'a, K, V> Reserve<'a, K, V>
where
    K: Clone + Eq + Hash,
{
    pub fn store(self, key: K, value: V) {
        let _delay = self.expirations.insert(key.clone(), self.expires);
        self.values.insert(key, value);
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
            let (mut cache, _cache_purge) = Cache::new(2, Duration::from_millis(10));

            let mut cache = match cache.poll_lock() {
                Async::Ready(acquired) => acquired,
                _ => panic!("cache lock should be Ready"),
            };

            {
                let slot = match cache.reserve() {
                    Ok(Async::Ready(slot)) => slot,
                    _ => panic!("cache should be Ready to reserve"),
                };
                slot.store(1, 2);
            }
            assert_eq!(cache.values.len(), 1);

            {
                let slot = match cache.reserve() {
                    Ok(Async::Ready(slot)) => slot,
                    _ => panic!("cache should be Ready to reserve"),
                };
                slot.store(2, 3);
            }
            assert_eq!(cache.values.len(), 2);

            {
                match cache.reserve() {
                    Ok(Async::NotReady) => (),
                    _ => panic!("cache should not be Ready to reserve"),
                }
            }
            assert_eq!(cache.values.len(), 2);

            Ok::<_, ()>(())
        }))
    }

    #[test]
    fn store_access_value() {
        current_thread::run(future::lazy(|| {
            let (mut cache, _cache_purge) = Cache::new(2, Duration::from_millis(10));

            let mut cache = match cache.poll_lock() {
                Async::Ready(acquired) => acquired,
                _ => panic!("cache lock should be Ready"),
            };

            assert!(cache.access(&1).is_none());
            assert!(cache.access(&2).is_none());

            {
                let slot = match cache.reserve() {
                    Ok(Async::Ready(slot)) => slot,
                    _ => panic!("cache should be Ready to reserve"),
                };
                slot.store(1, 2);
            }
            assert!(cache.access(&1).is_some());
            assert!(cache.access(&2).is_none());

            {
                let slot = match cache.reserve() {
                    Ok(Async::Ready(slot)) => slot,
                    _ => panic!("cache should be Ready to reserve"),
                };
                slot.store(2, 3);
            }
            assert!(cache.access(&1).is_some());
            assert!(cache.access(&2).is_some());

            assert_eq!(cache.access(&1).take().unwrap(), 2);
            assert_eq!(cache.access(&2).take().unwrap(), 3);

            Ok::<_, ()>(())
        }))
    }

    #[test]
    fn reserve_does_nothing_when_capacity_exists() {
        current_thread::run(futures::future::lazy(|| {
            let (mut cache, _cache_purge) = Cache::new(2, Duration::from_millis(10));

            let mut cache = match cache.poll_lock() {
                Async::Ready(acquired) => acquired,
                _ => panic!("cache lock should be Ready"),
            };

            {
                let slot = match cache.reserve() {
                    Ok(Async::Ready(slot)) => slot,
                    _ => panic!("cache should be Ready to reserve"),
                };
                slot.store(1, 2);
            }
            assert_eq!(cache.values.len(), 1);

            {
                let _slot = match cache.reserve() {
                    Ok(Async::Ready(slot)) => slot,
                    _ => panic!("cache should be Ready to reserve"),
                };
            }
            assert_eq!(cache.values.len(), 1);

            Ok::<_, ()>(())
        }))
    }

    #[test]
    fn store_and_self_purge() {
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
                let slot = match cache_1.reserve() {
                    Ok(Async::Ready(slot)) => slot,
                    _ => panic!("cache should be Ready to reserve"),
                };
                slot.store(1, 2);
            }
            {
                let slot = match cache_1.reserve() {
                    Ok(Async::Ready(slot)) => slot,
                    _ => panic!("cache should be Ready to reserve"),
                };
                slot.store(2, 3);
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
                let slot = match cache_2.reserve() {
                    Ok(Async::Ready(slot)) => slot,
                    _ => panic!("cache should be Ready to reserve"),
                };
                slot.store(3, 4);
            }
            assert_eq!(cache_2.values.len(), 2);
            assert!(cache_2.access(&3).is_some());

            Ok::<_, ()>(())
        }))
        .unwrap()
    }

    #[test]
    fn store_and_background_purge() {
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
                let slot = match cache.reserve() {
                    Ok(Async::Ready(slot)) => slot,
                    _ => panic!("cache should be Ready to reserve"),
                };
                slot.store(1, 2);
            }
            {
                let slot = match cache.reserve() {
                    Ok(Async::Ready(slot)) => slot,
                    _ => panic!("cache should be Ready to reserve"),
                };
                slot.store(2, 3);
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
    fn drop_access_resets_expiration() {
        let mut rt = current_thread::Runtime::new().unwrap();

        let (mut cache, cache_purge) = rt
            .block_on(futures::future::lazy(|| {
                Ok::<_, ()>(Cache::new(2, Duration::from_millis(10)))
            }))
            .unwrap();

        // Spawn a background purge task on the runtime
        rt.spawn(cache_purge);

        {
            let mut cache = match cache.poll_lock() {
                Async::Ready(acquired) => acquired,
                _ => panic!("cache lock should be Ready"),
            };

            // Hold on to an access handle the cache
            let _access = rt
                .block_on(future::lazy(|| {
                    {
                        let slot = match cache.reserve() {
                            Ok(Async::Ready(slot)) => slot,
                            _ => panic!("cache should be Ready to reserve"),
                        };
                        slot.store(1, 2);
                    }
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
            let mut cache = match cache.poll_lock() {
                Async::Ready(acquired) => acquired,
                _ => panic!("cache lock should be Ready"),
            };

            // The cache value should still be present since it was reset after
            // the value expiration. We ensured a background purge occurred but
            // that it did not purge the value.
            assert!(cache.access(&1).is_some());

            Ok::<_, ()>(())
        }))
        .unwrap()
    }
}
