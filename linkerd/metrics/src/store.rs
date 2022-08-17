use crate::{FmtLabels, FmtMetric, Metric};
use parking_lot::Mutex;
use std::{
    borrow::Borrow,
    collections::hash_map::{self, HashMap},
    fmt,
    hash::Hash,
    sync::Arc,
};
use tokio::time::Instant;

pub trait LastUpdate {
    fn last_update(&self) -> Instant;
}

pub type SharedStore<K, V> = Arc<Mutex<Store<K, V>>>;

#[derive(Debug)]
pub struct Store<K, V>
where
    K: Hash + Eq,
{
    inner: HashMap<K, Arc<V>>,
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

    pub fn get<Q>(&self, q: &Q) -> Option<&Arc<V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        self.inner.get(q)
    }

    pub fn entry(&mut self, key: K) -> hash_map::Entry<'_, K, Arc<V>> {
        self.inner.entry(key)
    }

    pub fn get_or_default(&mut self, k: K) -> &Arc<V>
    where
        V: Default,
    {
        self.inner.entry(k).or_default()
    }

    pub fn iter(&self) -> hash_map::Iter<'_, K, Arc<V>> {
        self.inner.iter()
    }

    pub fn retain_since(&mut self, epoch: Instant)
    where
        V: LastUpdate,
    {
        self.inner
            .retain(|_, metric| Arc::strong_count(metric) > 1 || metric.last_update() >= epoch)
    }

    /// Formats a metric across all instances of `Metrics` in the registry.
    pub fn fmt_by<N, M>(
        &self,
        f: &mut fmt::Formatter<'_>,
        metric: Metric<'_, N, M>,
        get_metric: impl Fn(&V) -> &M,
    ) -> fmt::Result
    where
        K: FmtLabels,
        N: fmt::Display,
        M: FmtMetric,
    {
        for (key, m) in self.iter() {
            get_metric(m).fmt_metric_labeled(f, &metric.name, key)?;
        }

        Ok(())
    }
}

impl<K, V> Store<K, Mutex<V>>
where
    K: Hash + Eq,
{
    /// Formats a metric across all instances of `Metrics` in the registry.
    pub fn fmt_by_locked<N, M>(
        &self,
        f: &mut fmt::Formatter<'_>,
        metric: Metric<'_, N, M>,
        get_metric: impl Fn(&V) -> &M,
    ) -> fmt::Result
    where
        K: FmtLabels,
        N: fmt::Display,
        M: FmtMetric,
    {
        for (key, m) in self.iter() {
            let m = m.lock();
            get_metric(&*m).fmt_metric_labeled(f, &metric.name, key)?;
        }

        Ok(())
    }
}

impl<K, V> Default for Store<K, V>
where
    K: Hash + Eq,
{
    fn default() -> Self {
        Self {
            inner: HashMap::new(),
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
