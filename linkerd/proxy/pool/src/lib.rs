//! Adapted from [`tower::buffer`][buffer].
//!
//! [buffer]: https://github.com/tower-rs/tower/tree/bf4ea948346c59a5be03563425a7d9f04aadedf2/tower/src/buffer
//
// Copyright (c) 2019 Tower Contributors

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use linkerd_metrics::prom;
use linkerd_stack::Service;

mod error;
mod failfast;
mod future;
mod message;
mod service;
#[cfg(test)]
mod tests;
mod worker;

pub use self::service::PoolQueue;
pub use linkerd_proxy_core::Update;

/// The buckets used to measure in-queue latency. We mostly want to get a
/// broad sense of overhead and not incur the costs of higher fidelity
/// histograms.
pub const LATENCY_BUCKETS: &[f64] = &[0.05, 0.1, 0.5, 1.0, 3.0, 10.0];

/// A collection of services updated from a resolution.
pub trait Pool<T, Req>: Service<Req> {
    /// Updates the pool's endpoints.
    fn update_pool(&mut self, update: Update<T>);

    /// Polls to update the pool while the Service is ready.
    ///
    /// [`Service::poll_ready`] should do the same work, but will return ready
    /// as soon as there at least one ready endpoint. This method will continue
    /// to drive the pool until ready is returned (indicating that the pool need
    /// not be updated before another request is processed).
    fn poll_pool(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>>;
}

#[derive(Clone, Debug)]
pub struct QueueMetricFamilies<L> {
    pub length: prom::Family<L, prom::Gauge>,
    pub request: prom::Family<L, prom::Counter>,
    pub latency: prom::Family<L, prom::Histogram, fn() -> prom::Histogram>,
}

// TODO(ver) load_avg.
#[derive(Clone, Debug)]
pub struct QueueMetrics {
    /// The number of queued requests.
    pub length: prom::Gauge,

    /// Counts the number of requests that enter the queue so we can measure
    /// "arrival rate" for a backend.
    pub request: prom::Counter,

    /// Measures the time spent in the queue--the latency added by Linkerd's
    /// balancer.
    pub latency: prom::Histogram,
}

// === impl QueueMetricsFamilies ===

impl<L> QueueMetricFamilies<L>
where
    L: prom::encoding::EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone + Send + Sync + 'static,
{
    pub fn register(reg: &mut prom::registry::Registry) -> Self {
        let length = prom::Family::default();
        reg.register(
            "length",
            "The current count of requests waiting in the queue",
            length.clone(),
        );

        let request = prom::Family::default();
        reg.register(
            "request",
            "The total number of requests that have entered the queue",
            request.clone(),
        );

        let latency = prom::Family::<_, _, fn() -> prom::Histogram>::new_with_constructor(|| {
            prom::Histogram::new(LATENCY_BUCKETS.iter().copied())
        });
        reg.register_with_unit(
            "latency",
            "The distribution of durations that requests have spent in the queue",
            prom::registry::Unit::Seconds,
            latency.clone(),
        );

        Self {
            length,
            request,
            latency,
        }
    }

    pub fn metrics(&self, labels: &L) -> QueueMetrics {
        let length = self.length.get_or_create(labels).clone();
        let request = self.request.get_or_create(labels).clone();
        let latency = self.latency.get_or_create(labels).clone();
        QueueMetrics {
            length,
            request,
            latency,
        }
    }
}

// === impl QueueMetrics ===

impl Default for QueueMetrics {
    fn default() -> Self {
        Self {
            length: prom::Gauge::default(),
            request: prom::Counter::default(),
            latency: prom::Histogram::new(LATENCY_BUCKETS.iter().copied()),
        }
    }
}
