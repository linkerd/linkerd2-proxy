use futures::{ready, TryFuture};
use linkerd_errno::Errno;
use linkerd_io as io;
use linkerd_metrics::{
    metrics, Counter, FmtLabels, FmtMetric, FmtMetrics, Gauge, LastUpdate, Metric, NewMetrics,
    Store,
};
use linkerd_stack::{layer, NewService, Param};
use parking_lot::Mutex;
use pin_project::pin_project;
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::hash::Hash;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::debug;

metrics! {
    tcp_open_total: Counter { "Total count of opened connections" },
    tcp_open_connections: Gauge { "Number of currently-open connections" },
    tcp_read_bytes_total: Counter { "Total count of bytes read from peers" },
    tcp_write_bytes_total: Counter { "Total count of bytes written to peers" },

    tcp_close_total: Counter { "Total count of closed connections" }
}

pub fn new<K: Eq + Hash + FmtLabels>(retain_idle: Duration) -> (Registry<K>, Report<K>) {
    let inner = Arc::new(Mutex::new(Inner::new()));
    let report = Report {
        metrics: inner.clone(),
        retain_idle,
    };
    (Registry(inner), report)
}

/// Implements `FmtMetrics` to render prometheus-formatted metrics for all transports.
#[derive(Clone, Debug)]
pub struct Report<K: Eq + Hash + FmtLabels> {
    metrics: Arc<Mutex<Inner<K>>>,
    retain_idle: Duration,
}

#[derive(Clone, Debug)]
pub struct Registry<K: Eq + Hash + FmtLabels>(Arc<Mutex<Inner<K>>>);

#[derive(Debug)]
pub struct ConnectLayer<K: Eq + Hash + FmtLabels> {
    registry: Arc<Mutex<Inner<K>>>,
}

pub type MakeAccept<N, K, S> = NewMetrics<N, K, Metrics, Accept<S>>;

#[derive(Clone, Debug)]
pub struct Accept<A> {
    inner: A,
    metrics: Arc<Metrics>,
}

#[derive(Debug)]
pub struct Connect<K: Eq + Hash + FmtLabels, M> {
    inner: M,
    registry: Arc<Mutex<Inner<K>>>,
}

#[pin_project]
pub struct Connecting<F> {
    #[pin]
    underlying: F,
    new_sensor: Option<NewSensor>,
}

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

/// Describes a classtransport end.
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

/// Tracks the state of a single instance of `Io` throughout its lifetime.
#[derive(Debug)]
pub struct Sensor {
    metrics: Option<Arc<Metrics>>,
    opened_at: Instant,
}

pub type SensorIo<T> = io::SensorIo<T, Sensor>;

/// Lazily builds instances of `Sensor`.
#[derive(Clone, Debug)]
struct NewSensor(Arc<Metrics>);

type Inner<K> = Store<K, Metrics>;

// ===== impl Registry =====

impl<K: Eq + Hash + FmtLabels> Registry<K> {
    pub fn layer_connect(&self) -> ConnectLayer<K> {
        ConnectLayer::new(self.0.clone())
    }

    pub fn layer_accept<M, T>(
        &self,
    ) -> impl layer::Layer<M, Service = MakeAccept<M, K, M::Service>> + Clone
    where
        M: NewService<T>,
    {
        MakeAccept::layer(self.0.clone())
    }
}

impl<K: Eq + Hash + FmtLabels> ConnectLayer<K> {
    fn new(registry: Arc<Mutex<Inner<K>>>) -> Self {
        Self { registry }
    }
}

impl<K: Eq + Hash + FmtLabels> Clone for ConnectLayer<K> {
    fn clone(&self) -> Self {
        Self::new(self.registry.clone())
    }
}

impl<K: Eq + Hash + FmtLabels, M> tower::layer::Layer<M> for ConnectLayer<K> {
    type Service = Connect<K, M>;

    fn layer(&self, inner: M) -> Self::Service {
        Connect {
            inner,
            registry: self.registry.clone(),
        }
    }
}

// === impl Accept ===

impl<I, A> tower::Service<I> for Accept<A>
where
    A: tower::Service<SensorIo<I>, Response = ()>,
{
    type Response = ();
    type Error = A::Error;
    type Future = A::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, io: I) -> Self::Future {
        let io = SensorIo::new(io, Sensor::open(self.metrics.clone()));
        self.inner.call(io)
    }
}

impl<A> From<(A, Arc<Metrics>)> for Accept<A> {
    fn from((inner, metrics): (A, Arc<Metrics>)) -> Self {
        Self { inner, metrics }
    }
}

// === impl Connect ===

impl<K, M> Clone for Connect<K, M>
where
    K: Eq + Hash + FmtLabels,
    M: Clone,
{
    fn clone(&self) -> Self {
        Connect {
            inner: self.inner.clone(),
            registry: self.registry.clone(),
        }
    }
}

impl<K, T, M> tower::Service<T> for Connect<K, M>
where
    T: Param<K>,
    K: Eq + Hash + FmtLabels,
    M: tower::make::MakeConnection<T>,
{
    type Response = SensorIo<M::Connection>;
    type Error = M::Error;
    type Future = Connecting<M::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let labels = target.param();
        let metrics = self.registry.lock().get_or_default(labels).clone();

        Connecting {
            new_sensor: Some(NewSensor(metrics)),
            underlying: self.inner.make_connection(target),
        }
    }
}

// === impl Connecting ===

impl<F> Future for Connecting<F>
where
    F: TryFuture,
    F::Ok: AsyncRead + AsyncWrite,
{
    type Output = Result<SensorIo<F::Ok>, F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let io = ready!(this.underlying.try_poll(cx))?;
        debug!("client connection open");

        let sensor = this
            .new_sensor
            .take()
            .expect("future must not be polled after ready")
            .new_sensor();
        let t = SensorIo::new(io, sensor);
        Poll::Ready(Ok(t))
    }
}

// ===== impl Report =====

impl<K: Eq + Hash + FmtLabels + 'static> Report<K> {
    /// Formats a metric across all instances of `EosMetrics` in the registry.
    fn fmt_eos_by<N, M>(
        inner: &Inner<K>,
        f: &mut fmt::Formatter<'_>,
        metric: Metric<'_, N, M>,
        get_metric: impl Fn(&EosMetrics) -> &M,
    ) -> fmt::Result
    where
        N: fmt::Display,
        M: FmtMetric,
    {
        for (key, metrics) in inner.iter() {
            let by_eos = (*metrics).by_eos.lock();
            for (eos, m) in by_eos.metrics.iter() {
                get_metric(&*m).fmt_metric_labeled(f, &metric.name, (key, eos))?;
            }
        }

        Ok(())
    }
}

impl<K: Eq + Hash + FmtLabels + 'static> FmtMetrics for Report<K> {
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut metrics = self.metrics.lock();
        if metrics.is_empty() {
            return Ok(());
        }

        tcp_open_total.fmt_help(f)?;
        metrics.fmt_by(f, tcp_open_total, |m| &m.open_total)?;

        tcp_open_connections.fmt_help(f)?;
        metrics.fmt_by(f, tcp_open_connections, |m| &m.open_connections)?;

        tcp_read_bytes_total.fmt_help(f)?;
        metrics.fmt_by(f, tcp_read_bytes_total, |m| &m.read_bytes_total)?;

        tcp_write_bytes_total.fmt_help(f)?;
        metrics.fmt_by(f, tcp_write_bytes_total, |m| &m.write_bytes_total)?;

        tcp_close_total.fmt_help(f)?;
        Self::fmt_eos_by(&*metrics, f, tcp_close_total, |e| &e.close_total)?;

        metrics.retain_since(Instant::now() - self.retain_idle);

        Ok(())
    }
}

// ===== impl Sensor =====

impl Sensor {
    fn open(metrics: Arc<Metrics>) -> Self {
        metrics.open_total.incr();
        metrics.open_connections.incr();
        metrics.by_eos.lock().last_update = Instant::now();

        Self {
            metrics: Some(metrics),
            opened_at: Instant::now(),
        }
    }
}

impl io::Sensor for Sensor {
    fn record_read(&mut self, sz: usize) {
        if let Some(ref m) = self.metrics {
            m.read_bytes_total.add(sz as u64);
            let mut by_eos = m.by_eos.lock();
            by_eos.last_update = Instant::now();
        }
    }

    fn record_write(&mut self, sz: usize) {
        if let Some(ref m) = self.metrics {
            m.write_bytes_total.add(sz as u64);
            let mut by_eos = m.by_eos.lock();
            by_eos.last_update = Instant::now();
        }
    }

    fn record_close(&mut self, eos: Option<Errno>) {
        // When closed, the metrics structure is dropped so that no further
        // updates can occur (i.e. so that an additional close won't be recorded
        // on Drop).
        if let Some(m) = self.metrics.take() {
            m.open_connections.decr();

            let mut by_eos = m.by_eos.lock();
            let class = by_eos
                .metrics
                .entry(Eos(eos))
                .or_insert_with(EosMetrics::default);
            class.close_total.incr();
            by_eos.last_update = Instant::now();
        }
    }

    /// Wraps an operation on the underlying transport with error telemetry.
    ///
    /// If the transport operation results in a non-recoverable error, record a
    /// transport closure.
    fn record_error<T>(&mut self, op: Poll<std::io::Result<T>>) -> Poll<std::io::Result<T>> {
        match op {
            Poll::Ready(Ok(v)) => Poll::Ready(Ok(v)),
            Poll::Ready(Err(e)) => {
                if e.kind() != std::io::ErrorKind::WouldBlock {
                    let eos = e.raw_os_error().map(|e| e.into());
                    self.record_close(eos);
                }

                Poll::Ready(Err(e))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for Sensor {
    fn drop(&mut self) {
        io::Sensor::record_close(self, None)
    }
}

// ===== impl NewSensor =====

impl NewSensor {
    fn new_sensor(self) -> Sensor {
        Sensor::open(self.0)
    }
}

// ===== impl Eos =====

impl FmtLabels for Eos {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            None => f.pad("errno=\"\""),
            Some(errno) => write!(f, "errno=\"{}\"", errno),
        }
    }
}

// ===== impl Metrics =====

impl LastUpdate for Metrics {
    fn last_update(&self) -> Instant {
        self.by_eos
            .try_lock()
            .map(|metrics| metrics.last_update)
            // if the metric is currently being updated, don't wait for it to be
            // done, just assume it's being updated now.
            .unwrap_or_else(Instant::now)
    }
}

// ===== impl ByEos =====

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
        use std::time::{Duration, Instant};

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
