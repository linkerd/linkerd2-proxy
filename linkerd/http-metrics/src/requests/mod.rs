mod report;
mod service;

pub use self::service::{NewHttpMetrics, ResponseBody};
use super::Report;
use linkerd_http_classify::ClassifyResponse;
use linkerd_metrics::{latency, Counter, FmtMetrics, Histogram, LastUpdate, NewMetrics};
use linkerd_stack::{self as svc, layer};
use std::{
    collections::HashMap,
    fmt::Debug,
    hash::Hash,
    time::{Duration, Instant},
};

type Registry<T, C> = super::Registry<T, Metrics<C>>;

#[derive(Debug)]
pub struct Requests<T, C>(Registry<T, C>)
where
    T: Hash + Eq,
    C: Hash + Eq;

#[derive(Debug)]
pub struct Metrics<C>
where
    C: Hash + Eq,
{
    last_update: Instant,
    total: Counter,
    by_status: HashMap<Option<http::StatusCode>, StatusMetrics<C>>,
}

#[derive(Debug)]
struct StatusMetrics<C>
where
    C: Hash + Eq,
{
    latency: Histogram<latency::Ms>,
    by_class: HashMap<C, ClassMetrics>,
}

#[derive(Debug, Default)]
pub struct ClassMetrics {
    total: Counter,
}

// === impl Requests ===

impl<T: Hash + Eq, C: Hash + Eq> Default for Requests<T, C> {
    fn default() -> Self {
        Requests(Registry::default())
    }
}

impl<T: Hash + Eq, C: Hash + Eq> Requests<T, C> {
    pub fn into_report(self, retain_idle: Duration) -> Report<T, Metrics<C>>
    where
        Report<T, Metrics<C>>: FmtMetrics,
    {
        Report::new(retain_idle, self.0)
    }

    pub fn to_layer<L, N, Tgt>(
        &self,
    ) -> impl layer::Layer<N, Service = NewHttpMetrics<N, T, L, C, N::Service>> + Clone
    where
        L: ClassifyResponse<Class = C> + Send + Sync + 'static,
        N: svc::NewService<Tgt>,
    {
        let reg = self.0.clone();
        NewMetrics::layer(reg)
    }
}

impl<T: Hash + Eq, C: Hash + Eq> Clone for Requests<T, C> {
    fn clone(&self) -> Self {
        Requests(self.0.clone())
    }
}

// === impl Metrics ===

impl<C: Hash + Eq> Default for Metrics<C> {
    fn default() -> Self {
        Self {
            last_update: Instant::now(),
            total: Counter::default(),
            by_status: HashMap::default(),
        }
    }
}

impl<C: Hash + Eq> LastUpdate for Metrics<C> {
    fn last_update(&self) -> Instant {
        self.last_update
    }
}

impl<C> Default for StatusMetrics<C>
where
    C: Hash + Eq,
{
    fn default() -> Self {
        Self {
            latency: Histogram::default(),
            by_class: HashMap::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn expiry() {
        use linkerd_metrics::FmtLabels;
        use std::fmt;
        use std::time::{Duration, Instant};

        #[derive(Clone, Debug, Hash, Eq, PartialEq)]
        struct Target(usize);
        impl FmtLabels for Target {
            fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "n=\"{}\"", self.0)
            }
        }

        #[allow(dead_code)]
        #[derive(Clone, Debug, Hash, Eq, PartialEq)]
        enum Class {
            Good,
            Bad,
        }

        impl FmtLabels for Class {
            fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                use std::fmt::Display;
                match self {
                    Class::Good => "class=\"good\"".fmt(f),
                    Class::Bad => "class=\"bad\"".fmt(f),
                }
            }
        }

        let retain_idle_for = Duration::from_secs(1);
        let r = super::Requests::<Target, Class>::default();
        let report = r.clone().into_report(retain_idle_for);
        let mut registry = r.0.lock();

        let before_update = Instant::now();
        let metrics = registry
            .entry(Target(123))
            .or_insert_with(Default::default)
            .clone();
        assert_eq!(registry.len(), 1, "target should be registered");
        let after_update = Instant::now();

        registry.retain_since(after_update);
        assert_eq!(
            registry.len(),
            1,
            "target should not be evicted by time alone"
        );

        drop(metrics);
        registry.retain_since(before_update);
        assert_eq!(
            registry.len(),
            1,
            "target should not be evicted by availability alone"
        );

        registry.retain_since(after_update);
        assert_eq!(
            registry.len(),
            0,
            "target should be evicted by time once the handle is dropped"
        );

        drop((registry, report));
    }
}
