//! Prometheus counters for request and response bodies.

use linkerd_metrics::prom::{
    self, metrics::family::MetricConstructor, Family, Histogram, Registry, Unit,
};

/// Counters for response body frames.
#[derive(Clone, Debug)]
pub struct ResponseBodyFamilies<L> {
    /// Counts the number of response body frames by size.
    frame_sizes: Family<L, Histogram, NewHisto>,
}

/// Counters to instrument a request or response body.
#[derive(Clone, Debug)]
pub struct BodyDataMetrics {
    /// Counts the number of request body frames.
    pub frame_size: Histogram,
}

#[derive(Clone, Copy)]
struct NewHisto;

impl MetricConstructor<Histogram> for NewHisto {
    fn new_metric(&self) -> Histogram {
        Histogram::new([128.0, 1024.0, 10240.0].into_iter())
    }
}

// === impl ResponseBodyFamilies ===

impl<L> Default for ResponseBodyFamilies<L>
where
    L: Clone + std::hash::Hash + Eq,
{
    fn default() -> Self {
        Self {
            frame_sizes: Family::new_with_constructor(NewHisto),
        }
    }
}

impl<L> ResponseBodyFamilies<L>
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
    /// Registers and returns a new family of body data metrics.
    pub fn register(registry: &mut Registry) -> Self {
        let frame_sizes = Family::new_with_constructor(NewHisto);
        registry.register_with_unit(
            "response_frame_size",
            "Response data frame sizes",
            Unit::Bytes,
            frame_sizes.clone(),
        );

        Self { frame_sizes }
    }

    /// Returns the [`BodyDataMetrics`] for the given label set.
    pub fn metrics(&self, labels: &L) -> BodyDataMetrics {
        let Self { frame_sizes } = self;

        let frame_size = frame_sizes.get_or_create(labels).clone();

        BodyDataMetrics { frame_size }
    }
}
