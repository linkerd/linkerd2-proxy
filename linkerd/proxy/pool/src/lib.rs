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

use self::failfast::{GateMetricFamilies, GateMetrics};

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
    length: prom::Family<L, prom::Gauge>,
    requests: prom::Family<L, prom::Counter>,
    latency: prom::Family<L, prom::Histogram, fn() -> prom::Histogram>,
    gate: GateMetricFamilies<L>,
}

// TODO(ver) load_avg.
#[derive(Clone, Debug)]
pub struct QueueMetrics {
    length: prom::Gauge,
    requests: prom::Counter,
    latency: prom::Histogram,
    gate: GateMetrics,
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

        let requests = prom::Family::default();
        reg.register(
            "requests",
            "The total number of requests that have entered the queue",
            requests.clone(),
        );

        let latency = prom::Family::<_, _, fn() -> prom::Histogram>::new_with_constructor(|| {
            // We mostly want to get a broad sense of overhead and not incur the
            // costs of higher fidelity histograms, so we use a constrained set
            // of buckets.
            prom::Histogram::new([0.0005, 0.005, 0.05, 0.5, 1.0, 3.0].iter().copied())
        });
        reg.register_with_unit(
            "latency",
            "The distribution of durations that requests have spent in the queue",
            prom::registry::Unit::Seconds,
            latency.clone(),
        );

        let gate = GateMetricFamilies::register(reg.sub_registry_with_prefix("gate"));

        Self {
            length,
            requests,
            latency,
            gate,
        }
    }

    pub fn metrics(&self, labels: &L) -> QueueMetrics {
        let length = self.length.get_or_create(labels).clone();
        let requests = self.requests.get_or_create(labels).clone();
        let latency = self.latency.get_or_create(labels).clone();
        let gate = self.gate.metrics(labels);
        QueueMetrics {
            length,
            requests,
            latency,
            gate,
        }
    }
}

// === impl QueueMetrics ===

impl Default for QueueMetrics {
    fn default() -> Self {
        Self {
            length: prom::Gauge::default(),
            requests: prom::Counter::default(),
            latency: prom::Histogram::new(std::iter::empty()),
            gate: GateMetrics::default(),
        }
    }
}
