mod io;

pub use self::io::Io;
use crate::metrics::{
    latency, metrics, Counter, FmtLabels, FmtMetric, FmtMetrics, Gauge, Histogram, Metric,
};
use crate::{svc, telemetry::Errno, transport::tls};
use futures::{try_ready, Future, Poll};
use indexmap::IndexMap;
use std::fmt;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{debug, error};

metrics! {
    tcp_open_total: Counter { "Total count of opened connections" },
    tcp_open_connections: Gauge { "Number of currently-open connections" },
    tcp_read_bytes_total: Counter { "Total count of bytes read from peers" },
    tcp_write_bytes_total: Counter { "Total count of bytes written to peers" },

    tcp_close_total: Counter { "Total count of closed connections" },
    tcp_connection_duration_ms: Histogram<latency::Ms> { "Connection lifetimes" }
}

pub fn new() -> (Registry, Report) {
    let inner = Arc::new(Mutex::new(Inner::default()));
    (Registry(inner.clone()), Report(inner))
}

/// Implements `FmtMetrics` to render prometheus-formatted metrics for all transports.
#[derive(Clone, Debug, Default)]
pub struct Report(Arc<Mutex<Inner>>);

#[derive(Clone, Debug, Default)]
pub struct Registry(Arc<Mutex<Inner>>);

#[derive(Debug)]
pub struct Accept {
    direction: Direction,
    registry: Arc<Mutex<Inner>>,
}

#[derive(Debug)]
pub struct LayerConnect<T, M> {
    direction: Direction,
    registry: Arc<Mutex<Inner>>,
    _p: PhantomData<fn() -> (T, M)>,
}

#[derive(Debug)]
pub struct Connect<T, M> {
    inner: M,
    direction: Direction,
    registry: Arc<Mutex<Inner>>,
    _p: PhantomData<fn() -> (T)>,
}

pub struct Connecting<F> {
    underlying: F,
    new_sensor: Option<NewSensor>,
}

/// Describes a class of transport.
///
/// A `Metrics` type exists for each unique `Key`.
///
/// Implements `FmtLabels`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
struct Key {
    direction: Direction,
    peer: Peer,
    tls_status: tls::Status,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
struct Direction(&'static str);

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
enum Peer {
    /// Represents the side of the proxy that accepts connections.
    Src,
    /// Represents the side of the proxy that opens connections.
    Dst,
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

    by_eos: IndexMap<Eos, EosMetrics>,
}

/// Describes a classtransport end.
///
/// An `EosMetrics` type exists for each unique `Key` and `Eos` pair.
///
/// Implements `FmtLabels`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Eos {
    Clean,
    Error(Errno),
}

/// Holds metrics for a class of end-of-stream.
#[derive(Debug, Default)]
struct EosMetrics {
    close_total: Counter,
    connection_duration: Histogram<latency::Ms>,
}

/// Tracks the state of a single instance of `Io` throughout its lifetime.
#[derive(Debug)]
struct Sensor {
    metrics: Option<Arc<Mutex<Metrics>>>,
    opened_at: Instant,
}

/// Lazily builds instances of `Sensor`.
#[derive(Clone, Debug)]
struct NewSensor(Option<Arc<Mutex<Metrics>>>);

/// Shares state between `Report` and `Registry`.
#[derive(Debug, Default)]
struct Inner(IndexMap<Key, Arc<Mutex<Metrics>>>);

// ===== impl Inner =====

impl Inner {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn iter(&self) -> impl Iterator<Item = (&Key, MutexGuard<'_, Metrics>)> {
        self.0
            .iter()
            .filter_map(|(k, l)| l.lock().ok().map(move |m| (k, m)))
    }

    /// Formats a metric across all instances of `Metrics` in the registry.
    fn fmt_by<F, M>(
        &self,
        f: &mut fmt::Formatter<'_>,
        metric: Metric<'_, M>,
        get_metric: F,
    ) -> fmt::Result
    where
        F: Fn(&Metrics) -> &M,
        M: FmtMetric,
    {
        for (key, m) in self.iter() {
            get_metric(&*m).fmt_metric_labeled(f, metric.name, key)?;
        }

        Ok(())
    }

    /// Formats a metric across all instances of `EosMetrics` in the registry.
    fn fmt_eos_by<F, M>(
        &self,
        f: &mut fmt::Formatter<'_>,
        metric: Metric<'_, M>,
        get_metric: F,
    ) -> fmt::Result
    where
        F: Fn(&EosMetrics) -> &M,
        M: FmtMetric,
    {
        for (key, metrics) in self.iter() {
            for (eos, m) in (*metrics).by_eos.iter() {
                get_metric(&*m).fmt_metric_labeled(f, metric.name, (key, eos))?;
            }
        }

        Ok(())
    }

    fn get_or_default(&mut self, k: Key) -> &Arc<Mutex<Metrics>> {
        self.0.entry(k).or_insert_with(|| Default::default())
    }
}

// ===== impl Registry =====

impl Registry {
    pub fn accept(&self, direction: &'static str) -> Accept {
        Accept {
            direction: Direction(direction),
            registry: self.0.clone(),
        }
    }

    pub fn connect<T, M>(&self, direction: &'static str) -> LayerConnect<T, M>
    where
        T: tls::HasPeerIdentity,
        M: svc::MakeConnection<T>,
    {
        LayerConnect::new(direction, self.0.clone())
    }
}

impl<I> crate::proxy::Accept<I> for Accept
where
    I: AsyncRead + AsyncWrite,
{
    type Io = Io<I>;

    fn accept(&self, source: &crate::proxy::Source, io: I) -> Self::Io {
        let tls_status = source.tls_peer.as_ref().map(|_| {}).into();
        let key = Key::accept(self.direction, tls_status);
        let metrics = match self.registry.lock() {
            Ok(mut inner) => Some(inner.get_or_default(key).clone()),
            Err(_) => {
                error!("unable to lock metrics registry");
                None
            }
        };
        Io::new(io, Sensor::open(metrics))
    }
}

impl<T, M> LayerConnect<T, M> {
    fn new(d: &'static str, registry: Arc<Mutex<Inner>>) -> Self {
        Self {
            direction: Direction(d),
            registry,
            _p: PhantomData,
        }
    }
}

impl<T, M> Clone for LayerConnect<T, M>
where
    T: Clone,
{
    fn clone(&self) -> Self {
        Self::new(self.direction.0, self.registry.clone())
    }
}

impl<T, M> svc::Layer<M> for LayerConnect<T, M>
where
    T: tls::HasPeerIdentity,
    M: svc::MakeConnection<T>,
{
    type Service = Connect<T, M>;

    fn layer(&self, inner: M) -> Self::Service {
        Connect {
            inner,
            direction: self.direction,
            registry: self.registry.clone(),
            _p: PhantomData,
        }
    }
}

impl<T, M> Clone for Connect<T, M>
where
    M: Clone,
{
    fn clone(&self) -> Self {
        Connect {
            inner: self.inner.clone(),
            direction: self.direction,
            registry: self.registry.clone(),
            _p: PhantomData,
        }
    }
}

/// impl MakeConnection
impl<T, M> svc::Service<T> for Connect<T, M>
where
    T: tls::HasPeerIdentity + Clone,
    M: svc::MakeConnection<T>,
{
    type Response = Io<M::Connection>;
    type Error = M::Error;
    type Future = Connecting<M::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        // TODO use target metadata in `key`
        let tls_status = target.peer_identity().as_ref().map(|_| ()).into();
        let key = Key::connect(self.direction, tls_status);
        let metrics = match self.registry.lock() {
            Ok(mut inner) => Some(inner.get_or_default(key).clone()),
            Err(_) => {
                error!("unable to lock metrics registry");
                None
            }
        };
        let underlying = self.inner.make_connection(target);

        Connecting {
            new_sensor: Some(NewSensor(metrics)),
            underlying,
        }
    }
}

// === impl Connecting ===

impl<F> Future for Connecting<F>
where
    F: Future,
    F::Item: AsyncRead + AsyncWrite,
{
    type Item = Io<F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let io = try_ready!(self.underlying.poll());
        debug!("client connection open");

        let sensor = self
            .new_sensor
            .take()
            .expect("future must not be polled after ready")
            .new_sensor();
        let t = Io::new(io, sensor);
        Ok(t.into())
    }
}

// ===== impl Report =====

impl FmtMetrics for Report {
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let metrics = match self.0.lock() {
            Err(_) => return Ok(()),
            Ok(lock) => lock,
        };

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
    pub fn open(metrics: Option<Arc<Mutex<Metrics>>>) -> Self {
        if let Some(ref m) = metrics {
            if let Ok(mut m) = m.lock() {
                m.open_total.incr();
                m.open_connections.incr();
            }
        }
        Self {
            metrics,
            opened_at: Instant::now(),
        }
    }

    pub fn record_read(&mut self, sz: usize) {
        if let Some(ref m) = self.metrics {
            if let Ok(mut m) = m.lock() {
                m.read_bytes_total += sz as u64;
            }
        }
    }

    pub fn record_write(&mut self, sz: usize) {
        if let Some(ref m) = self.metrics {
            if let Ok(mut m) = m.lock() {
                m.write_bytes_total += sz as u64;
            }
        }
    }

    pub fn record_close(&mut self, eos: Eos) {
        // When closed, the metrics structure is dropped so that no further
        // updates can occur (i.e. so that an additional close won't be recorded
        // on Drop).
        if let Some(m) = self.metrics.take() {
            let duration = self.opened_at.elapsed();
            if let Ok(mut m) = m.lock() {
                m.open_connections.decr();

                let class = m.by_eos.entry(eos).or_insert_with(|| EosMetrics::default());
                class.close_total.incr();
                class.connection_duration.add(duration);
            }
        }
    }
}

impl Drop for Sensor {
    fn drop(&mut self) {
        self.record_close(Eos::Clean)
    }
}

// ===== impl NewSensor =====

impl NewSensor {
    fn new_sensor(mut self) -> Sensor {
        Sensor::open(self.0.take())
    }
}

// ===== impl Key =====

impl Key {
    pub fn accept(direction: Direction, tls_status: tls::Status) -> Self {
        Self {
            peer: Peer::Src,
            direction,
            tls_status,
        }
    }

    pub fn connect(direction: Direction, tls_status: tls::Status) -> Self {
        Self {
            direction,
            peer: Peer::Dst,
            tls_status,
        }
    }
}

impl FmtLabels for Key {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        ((self.direction, self.peer), self.tls_status).fmt_labels(f)
    }
}

// ===== impl Peer =====

impl FmtLabels for Direction {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "direction=\"{}\"", self.0)
    }
}

// ===== impl Peer =====

impl FmtLabels for Peer {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Peer::Src => f.pad("peer=\"src\""),
            Peer::Dst => f.pad("peer=\"dst\""),
        }
    }
}

// ===== impl Eos =====

impl FmtLabels for Eos {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Eos::Clean => f.pad("errno=\"\""),
            Eos::Error(errno) => write!(f, "errno=\"{}\"", errno),
        }
    }
}
