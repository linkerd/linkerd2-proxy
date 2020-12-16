use crate::{FmtLabels, FmtMetric, Metric};
use std::{
    borrow::Borrow,
    collections::hash_map::{self, HashMap},
    fmt,
    hash::Hash,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub trait LastUpdate {
    fn last_update(&self) -> Instant;
}

pub trait FmtChildren<C> {
    type ChildLabels;
    fn with_children<F>(&self, f: F) -> fmt::Result
    where
        F: FnMut(&Self::ChildLabels, &C) -> fmt::Result;
}

#[derive(Debug)]
pub struct Store<K, V>
where
    K: Hash + Eq,
{
    inner: HashMap<K, Arc<V>>,
    retain_idle: Duration,
}

impl<K, V> Store<K, V>
where
    K: Hash + Eq,
{
    pub fn new(retain_idle: Duration) -> Self {
        Self {
            retain_idle,
            inner: HashMap::default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
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
        self.inner.entry(k).or_insert_with(|| Default::default())
    }

    pub fn iter(&self) -> hash_map::Iter<'_, K, Arc<V>> {
        self.inner.iter()
    }

    pub fn retain_active(&mut self)
    where
        V: LastUpdate,
    {
        let idle = self.retain_idle;
        self.inner.retain(|_, metric| {
            Arc::strong_count(&metric) > 1 || metric.last_update().elapsed() < idle
        })
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
            get_metric(&*m).fmt_metric_labeled(f, &metric.name, key)?;
        }

        Ok(())
    }

    pub fn fmt_children<N, M, C>(
        &self,
        f: &mut fmt::Formatter<'_>,
        metric: Metric<'_, N, M>,
        get_metric: impl Fn(&C) -> &M,
    ) -> fmt::Result
    where
        for<'a, 'b> (&'a K, &'b V::ChildLabels): FmtLabels,
        V: FmtChildren<C>,
        N: fmt::Display,
        M: FmtMetric,
    {
        for (key, metrics) in self.iter() {
            metrics.with_children(|child_key, child| {
                get_metric(child).fmt_metric_labeled(f, &metric.name, (key, child_key))
            })?;
        }

        Ok(())
    }
}

impl<M: LastUpdate> LastUpdate for Mutex<M> {
    fn last_update(&self) -> Instant {
        self.lock().unwrap().last_update()
    }
}

impl<M: LastUpdate> LastUpdate for Arc<M> {
    fn last_update(&self) -> Instant {
        std::ops::Deref::deref(self).last_update()
    }
}

impl<M: FmtChildren<C>, C> FmtChildren<C> for Mutex<M> {
    type ChildLabels = M::ChildLabels;

    fn with_children<F>(&self, f: F) -> fmt::Result
    where
        F: FnMut(&Self::ChildLabels, &C) -> fmt::Result,
    {
        if let Ok(lock) = self.lock() {
            lock.with_children(f)
        } else {
            Ok(())
        }
    }
}

impl<M: FmtChildren<C>, C> FmtChildren<C> for Arc<M> {
    type ChildLabels = M::ChildLabels;

    fn with_children<F>(&self, f: F) -> fmt::Result
    where
        F: FnMut(&Self::ChildLabels, &C) -> fmt::Result,
    {
        std::ops::Deref::deref(self).with_children(f)
    }
}
