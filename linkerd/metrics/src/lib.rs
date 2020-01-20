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
