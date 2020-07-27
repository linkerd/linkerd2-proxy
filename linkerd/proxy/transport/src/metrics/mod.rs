use futures::{ready, TryFuture};
use indexmap::IndexMap;
use linkerd2_errno::Errno;
use linkerd2_metrics::{
    latency, metrics, Counter, FmtLabels, FmtMetric, FmtMetrics, Gauge, Histogram, Metric,
};
use linkerd2_proxy_core as core;
use linkerd2_stack::layer;
use pin_project::pin_project;
use std::fmt;
use std::future::Future;
use std::hash::Hash;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::debug;

mod io;

pub use self::io::Io;

metrics! {
    tcp_open_total: Counter { "Total count of opened connections" },
    tcp_open_connections: Gauge { "Number of currently-open connections" },
    tcp_read_bytes_total: Counter { "Total count of bytes read from peers" },
    tcp_write_bytes_total: Counter { "Total count of bytes written to peers" },

    tcp_close_total: Counter { "Total count of closed connections" },
    tcp_connection_duration_ms: Histogram<latency::Ms> { "Connection lifetimes" }
}

pub fn new<K: Eq + Hash + FmtLabels>() -> (Registry<K>, Report<K>) {
    let inner = Arc::new(Mutex::new(Inner::default()));
    (Registry(inner.clone()), Report(inner))
}

pub trait TransportLabels<T> {
    type Labels: Hash + Eq + FmtLabels;

    fn transport_labels(&self, transport: &T) -> Self::Labels;
}

/// Implements `FmtMetrics` to render prometheus-formatted metrics for all transports.
#[derive(Clone, Debug, Default)]
pub struct Report<K: Eq + Hash + FmtLabels>(Arc<Mutex<Inner<K>>>);

#[derive(Clone, Debug, Default)]
pub struct Registry<K: Eq + Hash + FmtLabels>(Arc<Mutex<Inner<K>>>);

#[derive(Debug)]
pub struct ConnectLayer<L, K: Eq + Hash + FmtLabels> {
    label: L,
    registry: Arc<Mutex<Inner<K>>>,
}

#[derive(Debug)]
pub struct Accept<L, K: Eq + Hash + FmtLabels, M> {
    label: L,
    inner: M,
    registry: Arc<Mutex<Inner<K>>>,
}

#[derive(Debug)]
pub struct Connect<L, K: Eq + Hash + FmtLabels, M> {
    label: L,
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
///
/// TODO We should probaby use AtomicUsize for most of these counters so that
/// simple increments don't require a lock. Especially for read|write_bytes_total.
#[derive(Debug, Default)]
struct Metrics {
    open_total: Counter,
    open_connections: Gauge,
    write_bytes_total: Counter,
    read_bytes_total: Counter,

    by_eos: Arc<Mutex<IndexMap<Eos, EosMetrics>>>,
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
    connection_duration: Histogram<latency::Ms>,
}

/// Tracks the state of a single instance of `Io` throughout its lifetime.
#[derive(Debug)]
struct Sensor {
    metrics: Option<Arc<Metrics>>,
    opened_at: Instant,
}

/// Lazily builds instances of `Sensor`.
#[derive(Clone, Debug)]
struct NewSensor(Arc<Metrics>);

/// Shares state between `Report` and `Registry`.
#[derive(Debug)]
struct Inner<K: Eq + Hash + FmtLabels>(IndexMap<K, Arc<Metrics>>);

// ===== impl Inner =====

impl<K: Eq + Hash + FmtLabels> Default for Inner<K> {
    fn default() -> Self {
        Inner(IndexMap::default())
    }
}

impl<K: Eq + Hash + FmtLabels> Inner<K> {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn iter(&self) -> impl Iterator<Item = (&K, &Arc<Metrics>)> {
        self.0.iter()
    }

    /// Formats a metric across all instances of `Metrics` in the registry.
    fn fmt_by<F, N, M>(
        &self,
        f: &mut fmt::Formatter<'_>,
        metric: Metric<'_, N, M>,
        get_metric: F,
    ) -> fmt::Result
    where
        F: Fn(&Metrics) -> &M,
        N: fmt::Display,
        M: FmtMetric,
    {
        for (key, m) in self.iter() {
            get_metric(&*m).fmt_metric_labeled(f, &metric.name, key)?;
        }

        Ok(())
    }

    /// Formats a metric across all instances of `EosMetrics` in the registry.
    fn fmt_eos_by<F, N, M>(
        &self,
        f: &mut fmt::Formatter<'_>,
        metric: Metric<'_, N, M>,
        get_metric: F,
    ) -> fmt::Result
    where
        F: Fn(&EosMetrics) -> &M,
        N: fmt::Display,
        M: FmtMetric,
    {
        for (key, metrics) in self.iter() {
            if let Ok(by_eos) = (*metrics).by_eos.lock() {
                for (eos, m) in by_eos.iter() {
                    get_metric(&*m).fmt_metric_labeled(f, &metric.name, (key, eos))?;
                }
            }
        }

        Ok(())
    }

    fn get_or_default(&mut self, k: K) -> &Arc<Metrics> {
        self.0.entry(k).or_insert_with(|| Default::default())
    }
}

// ===== impl Registry =====

impl<K: Eq + Hash + FmtLabels> Registry<K> {
    pub fn layer_connect<L>(&self, label: L) -> ConnectLayer<L, K> {
        ConnectLayer::new(label, self.0.clone())
    }

    pub fn layer_accept<L: Clone, S>(
        &self,
        label: L,
    ) -> impl layer::Layer<S, Service = Accept<L, K, S>> + Clone {
        let registry = self.0.clone();
        layer::mk(move |inner| Accept {
            inner,
            label: label.clone(),
            registry: registry.clone(),
        })
    }

    // pub fn wrap_server_transport<T: AsyncRead + AsyncWrite>(&self, labels: K, io: T) -> Io<T> {
    //     let metrics = self
    //         .0
    //         .lock()
    //         .expect("metrics registry poisoned")
    //         .get_or_default(labels)
    //         .clone();
    //     Io::new(io, Sensor::open(metrics))
    // }
}

impl<L, K: Eq + Hash + FmtLabels> ConnectLayer<L, K> {
    fn new(label: L, registry: Arc<Mutex<Inner<K>>>) -> Self {
        Self { label, registry }
    }
}

impl<L: Clone, K: Eq + Hash + FmtLabels> Clone for ConnectLayer<L, K> {
    fn clone(&self) -> Self {
        Self::new(self.label.clone(), self.registry.clone())
    }
}

impl<L: Clone, K: Eq + Hash + FmtLabels, M> tower::layer::Layer<M> for ConnectLayer<L, K> {
    type Service = Connect<L, K, M>;

    fn layer(&self, inner: M) -> Self::Service {
        Connect {
            inner,
            label: self.label.clone(),
            registry: self.registry.clone(),
        }
    }
}

// === impl Accept ===

impl<L, K, M> Clone for Accept<L, K, M>
where
    L: Clone,
    K: Eq + Hash + FmtLabels,
    M: Clone,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            label: self.label.clone(),
            registry: self.registry.clone(),
        }
    }
}

impl<L, T, I, A> tower::Service<(T, I)> for Accept<L, L::Labels, A>
where
    L: TransportLabels<T>,
    A: core::Accept<(T, Io<I>)>,
{
    type Response = A::ConnectionFuture;
    type Error = A::Error;
    type Future = A::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, (target, io): (T, I)) -> Self::Future {
        let labels = self.label.transport_labels(&target);
        let metrics = self
            .registry
            .lock()
            .expect("metrics registr poisoned")
            .get_or_default(labels)
            .clone();

        self.inner
            .accept((target, Io::new(io, Sensor::open(metrics))))
    }
}

// === impl Connect ===

impl<L, K, M> Clone for Connect<L, K, M>
where
    L: Clone,
    K: Eq + Hash + FmtLabels,
    M: Clone,
{
    fn clone(&self) -> Self {
        Connect {
            inner: self.inner.clone(),
            label: self.label.clone(),
            registry: self.registry.clone(),
        }
    }
}

impl<L, T, M> tower::Service<T> for Connect<L, L::Labels, M>
where
    L: TransportLabels<T>,
    M: tower::make::MakeConnection<T>,
{
    type Response = Io<M::Connection>;
    type Error = M::Error;
    type Future = Connecting<M::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let labels = self.label.transport_labels(&target);
        let metrics = self
            .registry
            .lock()
            .expect("metrics registr poisoned")
            .get_or_default(labels)
            .clone();

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
    type Output = Result<Io<F::Ok>, F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let io = ready!(this.underlying.try_poll(cx))?;
        debug!("client connection open");

        let sensor = this
            .new_sensor
            .take()
            .expect("future must not be polled after ready")
            .new_sensor();
        let t = Io::new(io, sensor);
        Poll::Ready(Ok(t))
    }
}

// ===== impl Report =====

impl<K: Eq + Hash + FmtLabels> FmtMetrics for Report<K> {
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let metrics = self.0.lock().expect("metrics registry poisoned");
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
        metrics.fmt_eos_by(f, tcp_close_total, |e| &e.close_total)?;

        tcp_connection_duration_ms.fmt_help(f)?;
        metrics.fmt_eos_by(f, tcp_connection_duration_ms, |e| &e.connection_duration)?;

        Ok(())
    }
}

// ===== impl Sensor =====

impl Sensor {
    pub fn open(metrics: Arc<Metrics>) -> Self {
        metrics.open_total.incr();
        metrics.open_connections.incr();
        Self {
            metrics: Some(metrics),
            opened_at: Instant::now(),
        }
    }

    pub fn record_read(&mut self, sz: usize) {
        if let Some(ref m) = self.metrics {
            m.read_bytes_total.add(sz as u64);
        }
    }

    pub fn record_write(&mut self, sz: usize) {
        if let Some(ref m) = self.metrics {
            m.write_bytes_total.add(sz as u64);
        }
    }

    pub fn record_close(&mut self, eos: Option<Errno>) {
        let duration = self.opened_at.elapsed();
        // When closed, the metrics structure is dropped so that no further
        // updates can occur (i.e. so that an additional close won't be recorded
        // on Drop).
        if let Some(m) = self.metrics.take() {
            m.open_connections.decr();

            let mut by_eos = m.by_eos.lock().expect("transport eos metrics lock");
            let class = by_eos.entry(Eos(eos)).or_insert_with(EosMetrics::default);
            class.close_total.incr();
            class.connection_duration.add(duration);
        }
    }

    /// Wraps an operation on the underlying transport with error telemetry.
    ///
    /// If the transport operation results in a non-recoverable error, record a
    /// transport closure.
    fn sense_err<T>(&mut self, op: Poll<std::io::Result<T>>) -> Poll<std::io::Result<T>> {
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
        self.record_close(None)
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
