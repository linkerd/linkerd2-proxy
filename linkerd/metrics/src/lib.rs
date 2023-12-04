#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

//! Utilities for exposing metrics to Prometheus.

mod counter;
mod gauge;
mod histogram;
pub mod latency;
#[cfg(feature = "linkerd-stack")]
mod new_metrics;
mod prom;
mod serve;
mod store;

#[cfg(feature = "linkerd-stack")]
pub use self::new_metrics::NewMetrics;
pub use self::{
    counter::Counter,
    gauge::Gauge,
    histogram::Histogram,
    prom::{FmtLabels, FmtMetric, FmtMetrics, Metric},
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

mod value {
    use std::{
        marker::PhantomData,
        sync::atomic::{AtomicU64, Ordering},
    };

    /// Largest `u64` that can fit without loss of precision in `f64` (2^53).
    ///
    /// Wrapping is based on the fact that Prometheus models values as f64 (52-bits
    /// mantissa), thus integer values over 2^53 are not guaranteed to be correctly
    /// exposed.
    pub const MAX_PRECISE_UINT64: u64 = 0x20_0000_0000_0000;

    #[inline]
    pub fn u64_to_f64(n: u64) -> f64 {
        n.wrapping_rem(MAX_PRECISE_UINT64 + 1) as f64
    }

    #[inline]
    pub fn u128_to_u64(n: u128) -> u64 {
        n.wrapping_rem(MAX_PRECISE_UINT64 as u128 + 1) as u64
    }

    /// A sealed trait for Value types that can be exposed as Prometheus
    /// metrics. This is satisfied for Value<u64> and Value<f64>.
    pub trait PromValue {
        fn prom_value(&self) -> f64;
    }

    /// A metric value that handles Prometheus wrapping semantics.
    #[derive(Debug)]
    pub struct Value<T>(AtomicU64, PhantomData<T>);

    impl<V> Default for Value<V> {
        fn default() -> Self {
            Self(AtomicU64::default(), PhantomData)
        }
    }

    impl From<u64> for Value<u64> {
        fn from(n: u64) -> Self {
            Self(n.into(), PhantomData)
        }
    }

    impl From<f64> for Value<f64> {
        fn from(n: f64) -> Self {
            Self(n.to_bits().into(), PhantomData)
        }
    }

    impl Value<u64> {
        #[inline]
        pub fn add(&self, value: u64) {
            self.0.fetch_add(value, Ordering::Release);
        }

        #[inline]
        pub fn sub(&self, value: u64) {
            self.0.fetch_sub(value, Ordering::Release);
        }

        #[inline]
        pub fn set(&self, v: u64) {
            self.0.store(v, Ordering::Release)
        }

        #[inline]
        pub fn value(&self) -> u64 {
            self.0.load(Ordering::Acquire)
        }
    }

    impl PromValue for Value<u64> {
        fn prom_value(&self) -> f64 {
            u64_to_f64(self.value())
        }
    }

    impl Value<f64> {
        #[inline]
        pub fn set(&self, v: f64) {
            self.0.store(v.to_bits(), Ordering::Release)
        }

        #[inline]
        pub fn incr(&self) {
            self.add(1.0)
        }

        #[inline]
        pub fn decr(&self) {
            self.add(-1.0)
        }

        #[inline]
        pub fn add(&self, value: f64) {
            loop {
                let result = self
                    .0
                    .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |curr| {
                        let input = f64::from_bits(curr);
                        let output = input + value;
                        // On overflow, explicitly wrap to zero.
                        if output.is_infinite() || output.is_nan() {
                            Some(0)
                        } else {
                            Some(output.to_bits())
                        }
                    });

                if result.is_ok() {
                    break;
                }
            }
        }

        #[inline]
        pub fn value(&self) -> f64 {
            f64::from_bits(self.0.load(Ordering::Acquire))
        }
    }

    impl PromValue for Value<f64> {
        fn prom_value(&self) -> f64 {
            self.value()
        }
    }
}
