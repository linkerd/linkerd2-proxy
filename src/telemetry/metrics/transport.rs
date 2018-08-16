use indexmap::IndexMap;
use std::fmt;
use std::time::Duration;

use ctx;
use super::{
    labels::{Direction, TlsStatus},
    latency,
    prom::{FmtLabels, FmtMetrics},
    Counter,
    Gauge,
    Histogram,
};
use telemetry::Errno;

metrics! {
    tcp_open_total: Counter { "Total count of opened connections" },
    tcp_open_connections: Gauge { "Number of currently-open connections" },
    tcp_read_bytes_total: Counter { "Total count of bytes read from peers" },
    tcp_write_bytes_total: Counter { "Total count of bytes written to peers" },

    tcp_close_total: Counter { "Total count of closed connections" },
    tcp_connection_duration_ms: Histogram<latency::Ms> { "Connection lifetimes" }
}

/// Holds all transport stats.
///
/// Implements `fmt::Display` to render prometheus-formatted metrics for all transports.
#[derive(Debug, Default)]
pub struct Transports {
    metrics: IndexMap<Key, Metrics>,
}

/// Describes the dimensions across which transport metrics are aggregated.
///
/// Implements `fmt::Display` to render a comma-separated list of key-value pairs.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
struct Key {
    direction: Direction,
    peer: Peer,
    tls_status: TlsStatus,
}

/// Holds all of the metrics for a class of transport.
#[derive(Debug, Default)]
struct Metrics {
    open_total: Counter,
    open_connections: Gauge,
    write_bytes_total: Counter,
    read_bytes_total: Counter,

    by_eos: IndexMap<Eos, EosMetrics>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
enum Peer { Src, Dst }

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

// ===== impl Transports =====

impl Transports {
    pub fn open(&mut self, ctx: &ctx::transport::Ctx) {
        let metrics = self.metrics.entry(Key::new(ctx)).or_insert_with(|| Default::default());
        metrics.open_total.incr();
        metrics.open_connections.incr();
    }

    pub fn close(
        &mut self,
        ctx: &ctx::transport::Ctx,
        eos: Eos,
        duration: Duration,
        rx: u64,
        tx: u64,
    ) {
        let key = Key::new(ctx);

        let metrics = self.metrics.entry(key).or_insert_with(|| Default::default());
        metrics.open_connections.decr();
        metrics.read_bytes_total += rx;
        metrics.write_bytes_total += tx;

        let class = metrics.by_eos.entry(eos).or_insert_with(|| Default::default());
        class.close_total.incr();
        class.connection_duration.add(duration);
    }

    /// Iterates over all end-of-stream metrics.
    fn iter_eos(&self) -> impl Iterator<Item = ((&Key, &Eos), &EosMetrics)> {
        self.metrics
            .iter()
            .flat_map(|(k, t)| {
                t.by_eos.iter().map(move |(e, m)| ((k ,e), m))
            })
    }

    #[cfg(test)]
    pub fn open_total(&self, ctx: &ctx::transport::Ctx) -> u64 {
        self.metrics
            .get(&Key::new(ctx))
            .map(|m| m.open_total.into())
            .unwrap_or(0)
    }

    // #[cfg(test)]
    // pub fn open_connections(&self, ctx: &ctx::transport::Ctx) -> u64 {
    //     self.metrics
    //         .get(&Key::new(ctx))
    //         .map(|m| m.open_connections.into())
    //         .unwrap_or(0)
    // }

    #[cfg(test)]
    pub fn rx_tx_bytes_total(&self, ctx: &ctx::transport::Ctx) -> (u64, u64) {
        self.metrics
            .get(&Key::new(ctx))
            .map(|m| (m.read_bytes_total.into(), m.write_bytes_total.into()))
            .unwrap_or((0, 0))
    }

    #[cfg(test)]
    pub fn close_total(&self, ctx: &ctx::transport::Ctx, eos: Eos) -> u64 {
        self.metrics
            .get(&Key::new(ctx))
            .and_then(move |m| m.by_eos.get(&eos).map(|m| m.close_total.into()))
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub fn connection_durations(&self, ctx: &ctx::transport::Ctx, eos: Eos) -> Histogram<latency::Ms> {
        self.metrics
            .get(&Key::new(ctx))
            .and_then(move |m| m.by_eos.get(&eos).map(|m| m.connection_duration.clone()))
            .unwrap_or_default()
    }
}

impl FmtMetrics for Transports {
    fn fmt_metrics(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.metrics.is_empty() {
            return Ok(());
        }

        tcp_open_total.fmt_help(f)?;
        tcp_open_total.fmt_scopes(f, &self.metrics, |m| &m.open_total)?;

        tcp_open_connections.fmt_help(f)?;
        tcp_open_connections.fmt_scopes(f, &self.metrics, |m| &m.open_connections)?;

        tcp_read_bytes_total.fmt_help(f)?;
        tcp_read_bytes_total.fmt_scopes(f, &self.metrics, |m| &m.read_bytes_total)?;

        tcp_write_bytes_total.fmt_help(f)?;
        tcp_write_bytes_total.fmt_scopes(f, &self.metrics, |m| &m.write_bytes_total)?;

        tcp_close_total.fmt_help(f)?;
        tcp_close_total.fmt_scopes(f, self.iter_eos(), |e| &e.close_total)?;

        tcp_connection_duration_ms.fmt_help(f)?;
        tcp_connection_duration_ms.fmt_scopes(f, self.iter_eos(), |e| &e.connection_duration)?;

        Ok(())
    }
}

// ===== impl Key =====

impl FmtLabels for Key {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        ((self.direction, self.peer), self.tls_status).fmt_labels(f)
    }
}

impl Key {
    fn new(ctx: &ctx::transport::Ctx) -> Self {
        Self {
            direction: Direction::new(ctx.proxy()),
            peer: match *ctx {
                ctx::transport::Ctx::Server(_) => Peer::Src,
                ctx::transport::Ctx::Client(_) => Peer::Dst,
            },
            tls_status: ctx.tls_status().into(),
        }
    }
}

impl FmtLabels for Peer {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Peer::Src => f.pad("peer=\"src\""),
            Peer::Dst => f.pad("peer=\"dst\""),
        }
    }
}


// ===== impl Eos =====

impl FmtLabels for Eos {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self {
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
