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
    use metrics::family;
    pub use prometheus_client::{
        metrics::{
            counter::{ConstCounter, Counter},
            family::Family,
            gauge::{ConstGauge, Gauge},
            histogram::Histogram,
            info::Info,
        },
        registry::{Registry, Unit},
        *,
    };
    use std::sync::Arc;

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

    pub struct ScopedKey<'a, 'b>(pub &'a str, pub &'b str);

    impl encoding::EncodeLabelKey for ScopedKey<'_, '_> {
        fn encode(&self, enc: &mut encoding::LabelKeyEncoder<'_>) -> std::fmt::Result {
            use std::fmt::Write;
            write!(enc, "{}_{}", self.0, self.1)
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    pub struct Labels<A, B>(pub A, pub B);

    impl<A: EncodeLabelSetMut, B: EncodeLabelSetMut> EncodeLabelSetMut for Labels<A, B> {
        fn encode_label_set(&self, dst: &mut encoding::LabelSetEncoder<'_>) -> std::fmt::Result {
            self.0.encode_label_set(dst)?;
            self.1.encode_label_set(dst)
        }
    }

    impl<A: EncodeLabelSetMut, B: EncodeLabelSetMut> encoding::EncodeLabelSet for Labels<A, B> {
        fn encode(&self, mut dst: encoding::LabelSetEncoder<'_>) -> std::fmt::Result {
            self.0.encode_label_set(&mut dst)?;
            self.1.encode_label_set(&mut dst)
        }
    }

    #[derive(Clone, Debug)]
    pub struct NewHistogram(pub Arc<[f64]>);

    impl family::MetricConstructor<Histogram> for NewHistogram {
        fn new_metric(&self) -> Histogram {
            Histogram::new(self.0.iter().copied())
        }
    }

    pub type HistogramFamily<L> = family::Family<L, Histogram, NewHistogram>;

    pub fn histogram_family<L>(buckets: impl IntoIterator<Item = f64>) -> HistogramFamily<L>
    where
        L: Clone + std::hash::Hash + Eq,
    {
        family::Family::new_with_constructor(NewHistogram(buckets.into_iter().collect()))
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
