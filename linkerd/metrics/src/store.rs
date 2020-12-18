use crate::{FmtLabels, FmtMetric, Metric};
use std::{
    collections::hash_map,
    fmt,
    hash::Hash,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use dashmap::DashMap;

pub trait LastUpdate {
    fn last_update(&self) -> quanta::Instant;
}

#[derive(Debug, Default)]
pub struct UpdatedAt {
    last_update: AtomicU64,
}

pub type Registry<K, V> = Arc<Store<K, V>>;

pub fn registry<K, V>(clock: quanta::Clock) -> Registry<K, V>
where
    K: Hash + Eq,
{
    Registry::new(Store::new(clock))
}

// DashMap defaults to using `ahash` as the default hasher, which has
// unknown security properties --- its claims of DOS-resistance have not
// been peer-reviewed. Therefore, let's use the default hasher (SipHash) for
// metrics where keys may be exposed to untrusted input.
pub type Map<K, V> = DashMap<K, V, hash_map::RandomState>;
#[derive(Debug)]
pub struct Store<K, V>
where
    K: Hash + Eq,
{
    inner: Map<K, Arc<V>>,
    clock: quanta::Clock,
}

// === impl Store ===

impl<K, V> Store<K, V>
where
    K: Hash + Eq,
{
    pub fn new(clock: quanta::Clock) -> Self {
        let hasher = hash_map::RandomState::new();
        Self {
            inner: DashMap::with_hasher(hasher),
            clock,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn metric(&self, k: K) -> Arc<V>
    where
        V: From<quanta::Clock>,
    {
        self.inner
            .entry(k)
            .or_insert_with(|| Arc::new(V::from(self.clock.clone())))
            .clone()
    }

    pub fn iter(&self) -> dashmap::iter::Iter<'_, K, Arc<V>, hash_map::RandomState> {
        self.inner.iter()
    }

    pub fn retain_active(&self, idle: Duration)
    where
        V: LastUpdate,
    {
        self.retain_since(self.clock.recent() - idle)
    }

    pub fn retain_since(&self, epoch: quanta::Instant)
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
        for kv in self.iter() {
            let (key, m) = kv.pair();
            get_metric(&*m).fmt_metric_labeled(f, &metric.name, key)?;
        }

        Ok(())
    }

    pub fn clock(&self) -> &quanta::Clock {
        &self.clock
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
