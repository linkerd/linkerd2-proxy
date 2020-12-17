use crate::{FmtLabels, FmtMetric, Metric};
use std::{
    borrow::Borrow,
    collections::hash_map::{self, HashMap},
    fmt,
    hash::Hash,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

pub trait LastUpdate {
    fn last_update(&self) -> quanta::Instant;
}

#[derive(Debug, Default)]
pub struct UpdatedAt {
    last_update: AtomicU64,
}

#[derive(Debug)]
pub struct Store<K, V>
where
    K: Hash + Eq,
{
    inner: HashMap<K, Arc<V>>,
    clock: quanta::Clock,
}

impl<K, V> Store<K, V>
where
    K: Hash + Eq,
{
    pub fn new(clock: quanta::Clock) -> Self {
        Self {
            inner: Default::default(),
            clock,
        }
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

    pub fn get_or_insert(&mut self, k: K) -> Arc<V>
    where
        V: From<quanta::Clock>,
    {
        let clock = &self.clock;
        let inner = &mut self.inner;
        inner
            .entry(k)
            .or_insert_with(|| Arc::new(V::from(clock.clone())))
            .clone()
    }

    pub fn iter(&self) -> hash_map::Iter<'_, K, Arc<V>> {
        self.inner.iter()
    }

    pub fn retain_active(&mut self, idle: Duration)
    where
        V: LastUpdate,
    {
        self.retain_since(self.clock.recent() - idle)
    }

    pub fn retain_since(&mut self, epoch: quanta::Instant)
    where
        V: LastUpdate,
    {
        self.inner
            .retain(|_, metric| Arc::strong_count(&metric) > 1 || metric.last_update() >= epoch)
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

    pub fn clock(&self) -> &quanta::Clock {
        &self.clock
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
            let m = m.lock().unwrap();
            get_metric(&*m).fmt_metric_labeled(f, &metric.name, key)?;
        }

        Ok(())
    }
}

// === impl LastUpdate ===

impl<M: LastUpdate> LastUpdate for Mutex<M> {
    fn last_update(&self) -> quanta::Instant {
        self.lock().unwrap().last_update()
    }
}

impl<M: LastUpdate> LastUpdate for Arc<M> {
    fn last_update(&self) -> quanta::Instant {
        std::ops::Deref::deref(self).last_update()
    }
}

// === impl UpdatedAt ===

impl UpdatedAt {
    pub fn new(clock: &quanta::Clock) -> Self {
        let now = clock.recent().as_u64();
        Self {
            last_update: AtomicU64::new(now),
        }
    }

    pub fn update(&self, now: quanta::Instant) {
        self.last_update.fetch_max(now.as_u64(), Ordering::AcqRel);
    }

    pub fn last_update(&self, clock: &quanta::Clock) -> quanta::Instant {
        clock.scaled(self.last_update.load(Ordering::Acquire))
    }
}
