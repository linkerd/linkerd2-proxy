use linkerd_metrics::prom;

mod p2c;

pub use self::p2c::{P2cMetricFamilies, P2cMetrics, P2cPool};
pub use linkerd_proxy_pool::{Pool, QueueMetricFamilies, QueueMetrics, Update};

#[derive(Clone, Debug)]
pub struct MetricFamilies<L, U> {
    pub queue: QueueMetricFamilies<L>,
    pub p2c: P2cMetricFamilies<L, U>,
}

#[derive(Clone, Debug)]
pub struct Metrics {
    pub queue: QueueMetrics,
    pub p2c: P2cMetrics,
}

impl<L, U> MetricFamilies<L, U>
where
    L: prom::encoding::EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone + Send + Sync + 'static,
    U: prom::encoding::EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    U: Eq + Clone + Send + Sync + 'static,
{
    pub fn register(reg: &mut prom::registry::Registry) -> Self {
        let p2c = P2cMetricFamilies::register(reg.sub_registry_with_prefix("p2c"));
        let queue = QueueMetricFamilies::register(reg.sub_registry_with_prefix("queue"));
        Self { p2c, queue }
    }

    pub fn metrics<'l>(&self, labels: &'l L) -> Metrics
    where
        U: From<(Update<()>, &'l L)>,
    {
        tracing::trace!(?labels, "Budilding metrics");
        Metrics {
            p2c: self.p2c.metrics(&labels),
            queue: self.queue.metrics(&labels),
        }
    }
}
