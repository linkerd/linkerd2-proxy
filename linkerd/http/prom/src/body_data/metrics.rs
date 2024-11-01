//! Prometheus counters for request and response bodies.

use linkerd_metrics::prom::{self, Counter, Family, Registry};

/// Counters for response body frames.
#[derive(Clone, Debug)]
pub struct ResponseBodyFamilies<L> {
    /// Counts the number of response body frames.
    resp_body_frames_total: Family<L, Counter>,
    /// Counts the total number of bytes in response body frames.
    resp_body_frames_bytes: Family<L, Counter>,
}

/// Counters to instrument a request or response body.
#[derive(Clone, Debug, Default)]
pub struct BodyDataMetrics {
    /// Counts the number of request body frames.
    pub frames_total: Counter,
    /// Counts the total number of bytes in request body frames.
    pub frames_bytes: Counter,
}

// === impl ResponseBodyFamilies ===

impl<L> Default for ResponseBodyFamilies<L>
where
    L: Clone + std::hash::Hash + Eq,
{
    fn default() -> Self {
        Self {
            resp_body_frames_total: Default::default(),
            resp_body_frames_bytes: Default::default(),
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
    const RESP_BODY_FRAMES_TOTAL_NAME: &'static str = "resp_body_frames_total";
    const RESP_BODY_FRAMES_TOTAL_HELP: &'static str =
        "Counts the number of frames in response bodies.";

    const RESP_BODY_FRAMES_BYTES_NAME: &'static str = "resp_body_frames_bytes";
    const RESP_BODY_FRAMES_BYTES_HELP: &'static str =
        "Counts the total number of bytes in response bodies.";

    /// Registers and returns a new family of body data metrics.
    pub fn register(registry: &mut Registry) -> Self {
        let resp_body_frames_total = Family::default();
        registry.register(
            Self::RESP_BODY_FRAMES_TOTAL_NAME,
            Self::RESP_BODY_FRAMES_TOTAL_HELP,
            resp_body_frames_total.clone(),
        );

        let resp_body_frames_bytes = Family::default();
        registry.register_with_unit(
            Self::RESP_BODY_FRAMES_BYTES_NAME,
            Self::RESP_BODY_FRAMES_BYTES_HELP,
            prom::Unit::Bytes,
            resp_body_frames_bytes.clone(),
        );

        Self {
            resp_body_frames_total,
            resp_body_frames_bytes,
        }
    }

    /// Returns the [`BodyDataMetrics`] for the given label set.
    pub fn metrics(&self, labels: &L) -> BodyDataMetrics {
        let Self {
            resp_body_frames_total,
            resp_body_frames_bytes,
        } = self;

        let frames_total = resp_body_frames_total.get_or_create(labels).clone();
        let frames_bytes = resp_body_frames_bytes.get_or_create(labels).clone();

        BodyDataMetrics {
            frames_total,
            frames_bytes,
        }
    }
}
