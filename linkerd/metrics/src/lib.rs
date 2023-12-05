#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

//! Utilities for exposing metrics to Prometheus.

mod counter;
mod fmt;
mod gauge;
mod histogram;
pub mod latency;
#[cfg(feature = "linkerd-stack")]
mod new_metrics;
mod serve;
mod store;

#[cfg(feature = "linkerd-stack")]
pub use self::new_metrics::NewMetrics;
pub use self::{
    counter::Counter,
    fmt::{FmtLabels, FmtMetric, FmtMetrics, Metric},
    gauge::Gauge,
    histogram::Histogram,
    serve::Serve,
    store::{LastUpdate, SharedStore, Store},
};

/// Integration with the [`prometheus_client`]` crate.
///
/// This should be used for all new metrics.
pub mod prom {
    use parking_lot::RwLock;
    use std::sync::Arc;

    pub use prometheus_client::*;

    /// New metrics should use the prometheus-client Registry.
    pub type Registry = Arc<RwLock<registry::Registry>>;

    impl crate::FmtMetrics for Registry {
        #[inline]
        fn fmt_metrics(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            encoding::text::encode(f, &self.read())
        }
    }
}

#[macro_export]
macro_rules! metrics {
    { $( $name:ident : $kind:ty { $help:expr } ),+ } => {
        $(
            #[allow(non_upper_case_globals)]
            const $name: $crate::Metric<'static, &str, $kind> =
                $crate::Metric {
                    name: stringify!($name),
                    help: $help,
                    _p: ::std::marker::PhantomData,
                };
        )+
    }
}

pub trait Factor {
    fn factor(n: u64) -> f64;
}

pub struct MicrosAsSeconds;

pub struct MillisAsSeconds;

/// Largest `u64` that can fit without loss of precision in `f64` (2^53).
///
/// Wrapping is based on the fact that Prometheus models values as f64 (52-bits
/// mantissa), thus integer values over 2^53 are not guaranteed to be correctly
/// exposed.
const MAX_PRECISE_UINT64: u64 = 0x20_0000_0000_0000;

impl Factor for () {
    fn factor(n: u64) -> f64 {
        n.wrapping_rem(MAX_PRECISE_UINT64 + 1) as f64
    }
}

impl Factor for MillisAsSeconds {
    fn factor(n: u64) -> f64 {
        n.wrapping_rem((MAX_PRECISE_UINT64 + 1) * 1000) as f64 * 0.001
    }
}

impl Factor for MicrosAsSeconds {
    fn factor(n: u64) -> f64 {
        n.wrapping_rem((MAX_PRECISE_UINT64 + 1) * 1_000) as f64 * 0.000_001
    }
}
