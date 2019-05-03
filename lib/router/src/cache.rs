use futures::{Async, Future, Poll, Stream};
use indexmap::IndexMap;
use std::{
    hash::Hash,
    sync::{Arc, Mutex, TryLockError, Weak},
    time::Duration,
};
use tokio_timer::{delay_queue, DelayQueue, Error, Interval};

/// An LRU cache that is purged by a background purge task.
pub struct Cache<K: Clone + Eq + Hash, V> {
    capacity: usize,
    expires: Duration,
    expirations: DelayQueue<K>,
    vals: IndexMap<K, Node<V>>,
}

/// Purge a cache at a set interval.
pub struct PurgeCache<K: Clone + Eq + Hash, V> {
    cache: Weak<Mutex<Cache<K, V>>>,
    interval: Interval,
}

/// Wrap a cache node so that a lock is held on the entire cache. When the
/// access is dropped, reset the cache node so that it is not purged.
pub struct Access<'a, K: Clone + Eq + Hash, V> {
    expires: Duration,
    expirations: &'a mut DelayQueue<K>,
    pub(crate) node: &'a mut Node<V>,
}

/// This is the handle to a cache value.
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

    pub fn reserve(&mut self) -> Result<Reserve<K, V>, CapacityExhausted> {
        if self.vals.len() == self.capacity {
            match self.expirations.poll() {
                // The cache is at capacity but we are able to remove a value.
                Ok(Async::Ready(Some(entry))) => {
                    self.vals.remove(entry.get_ref());
                }

                // `Ready(None)` can only be returned when expirations is
                // empty. We know `expirations` is not empty because `vals` is
                // not empty.
                Ok(Async::Ready(None)) => unreachable!(),

                // The cache is at capacity and no values can be removed.
                Ok(Async::NotReady) => {
                    return Err(CapacityExhausted {
                        capacity: self.capacity,
                    });
                }

                // `warn!` could be helpful here in an actual use case.
                Err(_e) => {
                    return Err(CapacityExhausted {
                        capacity: self.capacity,
                    });
                }
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
            println!("error polling purge interval: {:?}", e);
            ()
        }));

        let lock = match self.cache.upgrade() {
            Some(lock) => lock,
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
            println!("error purging cache: {:?}", e);
            ()
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
