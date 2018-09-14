use indexmap::IndexMap;
use std::fmt;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncWrite};

use metrics::{
    latency,
    Counter,
    FmtLabels,
    FmtMetric,
    FmtMetrics,
    Gauge,
    Histogram,
    Metric,
};

use proxy;
use svc;
use telemetry::Errno;
use transport::{connect, tls};

mod io;

pub use self::io::{Connect, Connecting, Io};

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
pub struct LayerAccept<I, M> {
    direction: Direction,
    registry: Arc<Mutex<Inner>>,
    _p: PhantomData<fn() -> (I, M)>,
}

#[derive(Debug)]
pub struct StackAccept<I, M> {
    inner: M,
    direction: Direction,
    registry: Arc<Mutex<Inner>>,
    _p: PhantomData<fn() -> (I)>,
}

#[derive(Debug)]
pub struct Accept<I, A> {
    inner: A,
    metrics: Option<Arc<Mutex<Metrics>>>,
    _p: PhantomData<fn() -> (I)>,
}

#[derive(Debug)]
pub struct LayerConnect<T, M> {
    direction: Direction,
    registry: Arc<Mutex<Inner>>,
    _p: PhantomData<fn() -> (T, M)>,
}

#[derive(Debug)]
pub struct StackConnect<T, M> {
    inner: M,
    direction: Direction,
    registry: Arc<Mutex<Inner>>,
    _p: PhantomData<fn() -> (T)>,
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
    Error {
        errno: Option<Errno>,
    },
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

    fn iter(&self) -> impl Iterator<Item = (&Key, MutexGuard<Metrics>)> {
        self.0.iter()
            .filter_map(|(k, l)| l.lock().ok().map(move |m| (k, m)))
    }

    /// Formats a metric across all instances of `Metrics` in the registry.
    fn fmt_by<F, M>(&self, f: &mut fmt::Formatter, metric: Metric<M>, get_metric: F)
        -> fmt::Result
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
    fn fmt_eos_by<F, M>(&self, f: &mut fmt::Formatter, metric: Metric<M>, get_metric: F)
        -> fmt::Result
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

    pub fn accept<I, M>(&self, direction: &'static str) -> LayerAccept<I, M>
    where
        I: AsyncRead + AsyncWrite,
        M: svc::Stack<proxy::Source>,
        M::Value: proxy::Accept<I>,
    {
        LayerAccept::new(direction, self.0.clone())
    }

    pub fn connect<T, M>(&self, direction: &'static str) -> LayerConnect<T, M>
    where
        T: Into<connect::Target> + Clone,
        M: svc::Stack<T>,
        M::Value: connect::Connect,
    {
        LayerConnect::new(direction, self.0.clone())
    }
}

impl<I, M> LayerAccept<I, M>
where
    I: AsyncRead + AsyncWrite,
    M: svc::Stack<proxy::Source>,
    M::Value: proxy::Accept<I>,
{
    fn new(d: &'static str, registry: Arc<Mutex<Inner>>) -> Self {
        Self {
            direction: Direction(d),
            registry,
            _p: PhantomData,
        }
    }
}

impl<I, M> Clone for LayerAccept<I, M>
where
    I: AsyncRead + AsyncWrite,
    M: svc::Stack<proxy::Source>,
    M::Value: proxy::Accept<I>,
{
    fn clone(&self) -> Self {
        Self::new(self.direction.0, self.registry.clone())
    }
}

impl<I, M> svc::Layer<proxy::Source, proxy::Source, M> for LayerAccept<I, M>
where
    I: AsyncRead + AsyncWrite,
    M: svc::Stack<proxy::Source>,
    M::Value: proxy::Accept<I>
{
    type Value = <StackAccept<I, M> as svc::Stack<proxy::Source>>::Value;
    type Error = <StackAccept<I, M> as svc::Stack<proxy::Source>>::Error;
    type Stack = StackAccept<I, M>;

    fn bind(&self, inner: M) -> Self::Stack {
        StackAccept {
            inner,
            direction: self.direction,
            registry: self.registry.clone(),
            _p: PhantomData,
        }
    }
}

impl<I, M> Clone for StackAccept<I, M>
where
    I: AsyncRead + AsyncWrite,
    M: svc::Stack<proxy::Source> + Clone,
    M::Value: proxy::Accept<I>,
{
    fn clone(&self) -> Self {
        StackAccept {
            inner: self.inner.clone(),
            direction: self.direction,
            registry: self.registry.clone(),
            _p: PhantomData,
        }
    }
}

impl<I, M> svc::Stack<proxy::Source> for StackAccept<I, M>
where
    I: AsyncRead + AsyncWrite,
    M: svc::Stack<proxy::Source>,
    M::Value: proxy::Accept<I>
{
    type Value = Accept<I, M::Value>;
    type Error = M::Error;

    fn make(&self, source: &proxy::Source) -> Result<Self::Value, Self::Error> {
        // TODO use source metadata in `key`
        let key = Key::accept(self.direction, source.tls_status);
        let metrics = match self.registry.lock() {
            Ok(mut inner) => Some(inner.get_or_default(key).clone()),
            Err(_) => {
                error!("unable to lock metrics registry");
                None
            }
        };

        let inner = self.inner.make(&source)?;
        Ok(Accept {
            inner,
            metrics,
            _p: PhantomData,
        })
    }
}

impl<I, A> proxy::Accept<I> for Accept<I, A>
where
    I: AsyncRead + AsyncWrite,
    A: proxy::Accept<I>,
{
    type Io = Io<A::Io>;

    fn accept(&self, io: I) -> Self::Io {
        let io = self.inner.accept(io);
        Io::new(io, Sensor::open(self.metrics.clone()))
    }
}

impl<T, M> LayerConnect<T, M>
where
    T: Into<connect::Target> + Clone,
    M: svc::Stack<T>,
    M::Value: connect::Connect,
{
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
    T: Into<connect::Target> + Clone,
    M: svc::Stack<T>,
    M::Value: connect::Connect,
{
    fn clone(&self) -> Self {
        Self::new(self.direction.0, self.registry.clone())
    }
}

impl<T, M> svc::Layer<T, T, M> for LayerConnect<T, M>
where
    T: Into<connect::Target> + Clone,
    M: svc::Stack<T>,
    M::Value: connect::Connect,
{
    type Value = <StackConnect<T, M> as svc::Stack<T>>::Value;
    type Error = <StackConnect<T, M> as svc::Stack<T>>::Error;
    type Stack = StackConnect<T, M>;

    fn bind(&self, inner: M) -> Self::Stack {
        StackConnect {
            inner,
            direction: self.direction,
            registry: self.registry.clone(),
            _p: PhantomData,
        }
    }
}

impl<T, M> Clone for StackConnect<T, M>
where
    T: Into<connect::Target> + Clone,
    M: svc::Stack<T> + Clone,
    M::Value: connect::Connect,
{
    fn clone(&self) -> Self {
        StackConnect {
            inner: self.inner.clone(),
            direction: self.direction,
            registry: self.registry.clone(),
            _p: PhantomData,
        }
    }
}


impl<T, M> svc::Stack<T> for StackConnect<T, M>
where
    T: Into<connect::Target> + Clone,
    M: svc::Stack<T>,
    M::Value: connect::Connect,
{
    type Value = Connect<M::Value>;
    type Error = M::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        // TODO use target metadata in `key`
        let t: connect::Target = target.clone().into();
        let tls_status = t.tls.clone().map(|_| {});
        let key = Key::connect(self.direction, tls_status);
        let metrics = match self.registry.lock() {
            Ok(mut inner) => Some(inner.get_or_default(key).clone()),
            Err(_) => {
                error!("unable to lock metrics registry");
                None
            }
        };

        let inner = self.inner.make(&target)?;
        Ok(Connect::new(inner, NewSensor(metrics)))
    }
}


// ===== impl Report =====

impl FmtMetrics for Report {
    fn fmt_metrics(&self, f: &mut fmt::Formatter) -> fmt::Result {
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
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        ((self.direction, self.peer), self.tls_status).fmt_labels(f)
    }
}

// ===== impl Peer =====

impl FmtLabels for Direction {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "direction=\"{}\"", self.0)
    }
}

// ===== impl Peer =====

impl FmtLabels for Peer {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Peer::Src => f.pad("peer=\"src\""),
            Peer::Dst => f.pad("peer=\"dst\""),
        }
    }
}

// ===== impl Eos =====

impl FmtLabels for Eos {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Eos::Clean => f.pad("classification=\"success\""),
            Eos::Error { errno } => {
                f.pad("classification=\"failure\"")?;
                if let Some(e) = errno {
                    write!(f, ",errno=\"{}\"", e)?;
                }
                Ok(())
            }
        }
    }
}
