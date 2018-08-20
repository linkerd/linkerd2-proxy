use indexmap::IndexMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_connect;

use ctx;
use telemetry::Errno;
use telemetry::metrics::{
    latency,
    prom::{FmtLabels, FmtMetric, FmtMetrics, Metric},
    Counter,
    Gauge,
    Histogram,
};
use transport::Connection;

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
#[derive(Debug, Default)]
pub struct Report(Arc<Mutex<Inner>>);

/// Instruments transports to record telemetry.
#[derive(Clone, Debug, Default)]
pub struct Registry(Arc<Mutex<Inner>>);

/// Describes a class of transport.
///
/// A `Metrics` type exists for each unique `Key`.
///
/// Implements `FmtLabels`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
struct Key {
    proxy: ctx::Proxy,
    peer: Peer,
    tls_status: ctx::transport::TlsStatus,
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

/// Describes a class of transport end.
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

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
enum Peer {
    /// Represents the side of the proxy that accepts connections.
    Src,
    /// Represents the side of the proxy that opens connections.
    Dst,
}

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

    pub fn new_connect<C>(&self, ctx: &ctx::transport::Client, inner: C) -> Connect<C>
    where
        C: tokio_connect::Connect<Connected = Connection>,
    {
        let metrics = match self.0.lock() {
            Ok(mut inner) => {
                Some(inner.get_or_default(Key::client(ctx)).clone())
            }
            Err(_) => {
                error!("unable to lock metrics registry");
                None
            }
        };
        Connect::new(inner, NewSensor(metrics))
    }

    pub fn accept<T>(&self, ctx: &ctx::transport::Server, io: T) -> Io<T>
    where
        T: AsyncRead + AsyncWrite,
    {
        let metrics = match self.0.lock() {
            Ok(mut inner) => Some(inner.get_or_default(Key::server(ctx)).clone()),
            Err(_) => {
                error!("unable to lock metrics registry");
                None
            }
        };
        Io::new(io, Sensor::open(metrics))
    }
}

#[cfg(test)]
impl Registry {

    pub fn open_total(&self, ctx: &ctx::transport::Ctx) -> u64 {
        self.0.lock().unwrap().0
            .get(&Key::new(ctx))
            .map(|m| m.lock().unwrap().open_total.into())
            .unwrap_or(0)
    }

    // pub fn open_connections(&self, ctx: &ctx::transport::Ctx) -> u64 {
    //    self.0.lock().unwrap().0
    //         .get(&Key::new(ctx))
    //         .map(|m| m.lock().unwrap().open_connections.into())
    //         .unwrap_or(0)
    // }

    pub fn rx_tx_bytes_total(&self, ctx: &ctx::transport::Ctx) -> (u64, u64) {
        self.0.lock().unwrap().0
            .get(&Key::new(ctx))
            .map(|m| {
                let m = m.lock().unwrap();
                (m.read_bytes_total.into(), m.write_bytes_total.into())
            })
            .unwrap_or((0, 0))
    }

    pub fn close_total(&self, ctx: &ctx::transport::Ctx, eos: Eos) -> u64 {
        self.0.lock().unwrap().0
            .get(&Key::new(ctx))
            .and_then(move |m| m.lock().unwrap().by_eos.get(&eos).map(|m| m.close_total.into()))
            .unwrap_or(0)
    }

    pub fn connection_durations(&self, ctx: &ctx::transport::Ctx, eos: Eos) -> Histogram<latency::Ms> {
        self.0.lock().unwrap().0
            .get(&Key::new(ctx))
            .and_then(move |m| {
                m.lock().unwrap().by_eos.get(&eos).map(|m| m.connection_duration.clone())
            })
            .unwrap_or_default()
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
    #[cfg(test)]
    fn new(ctx: &ctx::transport::Ctx) -> Self {
        match ctx {
            ctx::transport::Ctx::Client(ctx) => Self::client(ctx),
            ctx::transport::Ctx::Server(ctx) => Self::server(ctx),
        }
    }

    fn client(ctx: &ctx::transport::Client) -> Self {
        Self {
            proxy: ctx.proxy,
            peer: Peer::Dst,
            tls_status: ctx.tls_status.into(),
        }
    }

    fn server(ctx: &ctx::transport::Server) -> Self {
        Self {
            proxy: ctx.proxy,
            peer: Peer::Src,
            tls_status: ctx.tls_status.into(),
        }
    }
}

impl FmtLabels for Key {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        ((self.proxy, self.peer), self.tls_status).fmt_labels(f)
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
