#![deny(
    warnings,
    rust_2018_idioms,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]
#![forbid(unsafe_code)]

mod client;
mod report;
mod sensor;
mod server;

pub use self::{
    client::Client,
    report::Report,
    sensor::{Sensor, SensorIo},
    server::NewServer,
};
use linkerd_errno::Errno;
use linkerd_metrics::{metrics, Counter, FmtLabels, Gauge, LastUpdate, Store};
use parking_lot::Mutex;
use std::{collections::HashMap, fmt, hash::Hash, sync::Arc};
use tokio::time::{Duration, Instant};

metrics! {
    tcp_open_total: Counter { "Total count of opened connections" },
    tcp_open_connections: Gauge { "Number of currently-open connections" },
    tcp_read_bytes_total: Counter { "Total count of bytes read from peers" },
    tcp_write_bytes_total: Counter { "Total count of bytes written to peers" },

    tcp_close_total: Counter { "Total count of closed connections" }
}

pub fn new<K: Eq + Hash + FmtLabels>(retain_idle: Duration) -> (Registry<K>, Report<K>) {
    let inner = Arc::new(Mutex::new(Inner::new()));
    let report = Report::new(inner.clone(), retain_idle);
    (Registry(inner), report)
}

#[derive(Clone, Debug)]
pub struct Registry<K: Eq + Hash + FmtLabels>(Arc<Mutex<Inner<K>>>);

type Inner<K> = Store<K, Metrics>;

/// Stores a class of transport's metrics.
#[derive(Debug, Default)]
pub struct Metrics {
    open_total: Counter,
    open_connections: Gauge,
    write_bytes_total: Counter,
    read_bytes_total: Counter,

    by_eos: Arc<Mutex<ByEos>>,
}

#[derive(Debug)]
struct ByEos {
    last_update: Instant,
    metrics: HashMap<Eos, EosMetrics>,
}

/// Describes a class of transport end.
///
/// An `EosMetrics` type exists for each unique `Key` and `Eos` pair.
///
/// Implements `FmtLabels`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
struct Eos(Option<Errno>);

/// Holds metrics for a class of end-of-stream.
#[derive(Debug, Default)]
struct EosMetrics {
    close_total: Counter,
}

// === impl Registry ===

impl<K: Eq + Hash + FmtLabels> Registry<K> {
    pub fn metrics(&self, labels: K) -> Arc<Metrics> {
        self.0.lock().get_or_default(labels).clone()
    }
}

// === impl Eos ===

impl FmtLabels for Eos {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            None => f.pad("errno=\"\""),
            Some(errno) => write!(f, "errno=\"{}\"", errno),
        }
    }
}

// === impl Metrics ===

impl LastUpdate for Metrics {
    fn last_update(&self) -> Instant {
        self.by_eos.lock().last_update
    }
}

// === impl ByEos ===

impl Default for ByEos {
    fn default() -> Self {
        Self {
            metrics: HashMap::new(),
            last_update: Instant::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn expiry() {
        use linkerd_metrics::FmtLabels;
        use std::fmt;
        use tokio::time::{Duration, Instant};

        #[derive(Clone, Debug, Hash, Eq, PartialEq)]
        struct Target(usize);
        impl FmtLabels for Target {
            fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "n=\"{}\"", self.0)
            }
        }

        let retain_idle_for = Duration::from_secs(1);
        let (r, report) = super::new(retain_idle_for);
        let mut registry = r.0.lock();

        let before_update = Instant::now();
        let metrics = registry.entry(Target(123)).or_default().clone();
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
