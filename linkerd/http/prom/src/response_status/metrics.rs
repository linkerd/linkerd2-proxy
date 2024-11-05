//! Prometheus counters for tracking response status codes.

use linkerd_metrics::prom::{self, Counter, Family, Registry};

/// Counters for response status codes.
#[derive(Clone, Debug)]
pub struct ResponseStatusFamilies<L> {
    statuses: Family<L, Counter>,
}

// === impl ResponseStatusFamilies ===

impl<L> Default for ResponseStatusFamilies<L>
where
    L: Clone + std::hash::Hash + Eq,
{
    fn default() -> Self {
        Self {
            statuses: Default::default(),
        }
    }
}

impl<L> ResponseStatusFamilies<L>
where
    L: prom::encoding::EncodeLabelSet
        + std::fmt::Debug
        + std::hash::Hash
        + Eq
        + Clone
        + Send
        + Sync
        + 'static,
{
    /// Registers and returns a new family of statuc code metrics.
    pub fn register(registry: &mut Registry) -> Self {
        let families = Self::default();
        let Self { statuses } = &families;
        registry.register("response_statuses", "Completed responses", statuses.clone());
        families
    }

    /// Returns the counter for the given labels.
    pub fn get(&self, labels: &L) -> Counter {
        self.statuses.get_or_create(labels).clone()
    }
}
