use dashmap::DashMap;
use parking_lot::Mutex;
use std::{hash::Hash, sync::Arc};
use tokio::time::Instant;

pub trait LastUpdate {
    fn last_update(&self) -> Instant;
}

pub type SharedStore<K, V> = Arc<Store<K, V>>;

/// A point-in-time copy of the store's contents.
pub type Snapshot<K, V> = Vec<(Arc<K>, Arc<V>)>;

#[derive(Debug)]
pub struct Store<K, V>
where
    K: Hash + Eq,
{
    // A concurrent hashmap of metric labels and values.
    // Each key is wrapped in an Arc to avoid expensive clones
    // during snapshots.
    inner: DashMap<Arc<K>, Arc<V>>,
}

impl<K, V> Store<K, V>
where
    K: Hash + Eq,
{
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn get(&self, key: &K) -> Option<Arc<V>> {
        self.inner.get(key).map(|r| r.value().clone())
    }

    pub fn get_or_default(&self, key: K) -> Arc<V>
    where
        V: Default,
    {
        if let Some(metric) = self.inner.get(&key).map(|r| r.value().clone()) {
            return metric;
        }
        // Note that the guard from the get() call is dropped to avoid a deadlock.
        self.inner.entry(Arc::new(key)).or_default().value().clone()
    }

    pub fn snapshot(&self) -> Snapshot<K, V> {
        self.inner
            .iter()
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect()
    }

    pub fn retain_since(&self, epoch: Instant)
    where
        V: LastUpdate,
    {
        self.inner
            .retain(|_, metric| Arc::strong_count(metric) > 1 || metric.last_update() >= epoch)
    }
}

impl<K, V> Default for Store<K, V>
where
    K: Hash + Eq,
{
    fn default() -> Self {
        Self {
            inner: DashMap::new(),
        }
    }
}

// === impl LastUpdate ===

impl<M: LastUpdate> LastUpdate for Mutex<M> {
    fn last_update(&self) -> Instant {
        self.lock().last_update()
    }
}

impl<M: LastUpdate> LastUpdate for Arc<M> {
    fn last_update(&self) -> Instant {
        std::ops::Deref::deref(self).last_update()
    }
}
