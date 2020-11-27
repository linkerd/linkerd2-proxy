#![deny(warnings, rust_2018_idioms)]

//! Utilties for exposing metrics to Prometheus.

mod counter;
mod gauge;
mod histogram;
pub mod latency;
mod prom;
mod scopes;
mod serve;

pub use self::counter::Counter;
pub use self::gauge::Gauge;
pub use self::histogram::Histogram;
pub use self::prom::{FmtLabels, FmtMetric, FmtMetrics, Metric};
pub use self::scopes::Scopes;
pub use self::serve::Serve;

#[macro_export]
macro_rules! metrics {
    { $( $name:ident : $kind:ty { $help:expr } ),+ } => {
        $(
            #[allow(non_upper_case_globals)]
            const $name: ::linkerd2_metrics::Metric<'static, &str, $kind> =
                ::linkerd2_metrics::Metric {
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
