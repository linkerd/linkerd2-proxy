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
#[cfg(feature = "process")]
pub mod process;
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
    use std::sync::Arc;

    pub use prometheus_client::{
        metrics::{
            counter::{ConstCounter, Counter},
            family::Family,
            gauge::{Atomic as GaugeAtomic, ConstGauge, Gauge},
            histogram::Histogram,
            info::Info,
        },
        registry::{Registry, Unit},
        *,
    };

    pub trait EncodeLabelSetMut: encoding::EncodeLabelSet {
        fn encode_label_set(&self, dst: &mut encoding::LabelSetEncoder<'_>) -> std::fmt::Result;
    }

    pub type Report = Arc<Registry>;

    impl crate::FmtMetrics for Report {
        #[inline]
        fn fmt_metrics(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            encoding::text::encode(f, self)
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
