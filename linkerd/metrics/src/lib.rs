#![deny(
    warnings,
    rust_2018_idioms,
    clippy::disallowed_method,
    clippy::disallowed_type
)]
#![forbid(unsafe_code)]

//! Utilities for exposing metrics to Prometheus.

mod counter;
mod gauge;
mod histogram;
pub mod latency;
#[cfg(feature = "linkerd-stack")]
mod new_metrics;
mod prom;
mod scopes;
mod serve;
mod store;
#[cfg(feature = "summary")]
mod summary;

#[cfg(feature = "linkerd-stack")]
pub use self::new_metrics::NewMetrics;
#[cfg(feature = "summary")]
pub use self::summary::Summary;
pub use self::{
    counter::Counter,
    gauge::Gauge,
    histogram::Histogram,
    prom::{FmtLabels, FmtMetric, FmtMetrics, Metric},
    scopes::Scopes,
    serve::Serve,
    store::{LastUpdate, SharedStore, Store},
};

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
