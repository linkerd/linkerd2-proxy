//! Utilties for exposing metrics to Prometheus.

extern crate deflate;
extern crate futures;
extern crate http;
extern crate hyper;
extern crate indexmap;
#[macro_use]
extern crate log;
#[cfg(test)]
#[macro_use]
extern crate quickcheck;

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
            const $name: ::linkerd2_metrics::Metric<'static, $kind> =
                ::linkerd2_metrics::Metric {
                    name: stringify!($name),
                    help: $help,
                    _p: ::std::marker::PhantomData,
                };
        )+
    }
}
