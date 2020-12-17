use super::{LastUpdate, Report};
use linkerd2_http_classify::ClassifyResponse;
use linkerd2_metrics::{
    latency,
    store::{self, Store},
    Counter, FmtMetrics, Histogram,
};
use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod layer;
mod report;

pub use self::layer::ResponseBody;

type SharedRegistry<T, C> = Arc<Mutex<Store<T, Metrics<C>>>>;

#[derive(Debug)]
pub struct Requests<T, C>(SharedRegistry<T, C>)
where
    T: Hash + Eq,
    C: Hash + Eq;

#[derive(Debug)]
pub struct Metrics<C>
where
    C: Hash + Eq,
{
    last_update: store::UpdatedAt,
    total: Counter,
    clock: quanta::Clock,
    by_status: Mutex<HashMap<Option<http::StatusCode>, StatusMetrics<C>>>,
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

impl<T: Hash + Eq, C: Hash + Eq> Requests<T, C> {
    pub fn new(clock: quanta::Clock) -> Self {
        Requests(Arc::new(Mutex::new(Store::new(clock))))
    }

    pub fn into_report(self, retain_idle: Duration) -> Report<T, Metrics<C>>
    where
        Report<T, Metrics<C>>: FmtMetrics,
    {
        Report::new(retain_idle, self.0)
    }

    pub fn into_layer<L>(self) -> layer::Layer<T, L>
    where
        L: ClassifyResponse<Class = C> + Send + Sync + 'static,
    {
        layer::Layer::new(self.0)
    }
}

impl<T: Hash + Eq, C: Hash + Eq> Clone for Requests<T, C> {
    fn clone(&self) -> Self {
        Requests(self.0.clone())
    }
}

// === impl Metrics ===

impl<C: Hash + Eq> From<quanta::Clock> for Metrics<C> {
    fn from(clock: quanta::Clock) -> Self {
        let last_update = store::UpdatedAt::new(&clock);
        Self {
            last_update,
            clock,
            total: Default::default(),
            by_status: Mutex::new(Default::default()),
        }
    }
}

impl<C: Hash + Eq> LastUpdate for Metrics<C> {
    fn last_update(&self) -> quanta::Instant {
        self.last_update.last_update(&self.clock)
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
        use linkerd2_metrics::FmtLabels;
        use std::fmt;
        use std::time::Duration;

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
        };
        impl FmtLabels for Class {
            fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                use std::fmt::Display;
                match self {
                    Class::Good => "class=\"good\"".fmt(f),
                    Class::Bad => "class=\"bad\"".fmt(f),
                }
            }
        }

        let (clock, time) = quanta::Clock::mock();

        let retain_idle_for = Duration::from_secs(10);
        let r = super::Requests::<Target, Class>::new(clock.clone());
        let report = r.clone().into_report(retain_idle_for);
        let mut registry = r.0.lock().unwrap();

        let before_update = clock.recent();
        let metrics = registry.get_or_insert(Target(123));
        assert_eq!(registry.len(), 1, "target should be registered");
        time.increment(retain_idle_for);
        let after_update = clock.recent();

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
