use std::fmt;
use std::time::Duration;

use ctx;
use super::{
    labels::{Direction, TlsStatus},
    latency,
    Counter,
    Gauge,
    Histogram,
    Scopes,
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
    opens: OpenScopes,
    closes: CloseScopes,
}

type OpenScopes = Scopes<TransportLabels, OpenMetrics>;

#[derive(Debug, Default)]
struct OpenMetrics {
    open_total: Counter,
    open_connections: Gauge,
    write_bytes_total: Counter,
    read_bytes_total: Counter,
}

type CloseScopes = Scopes<TransportCloseLabels, CloseMetrics>;

/// Labels describing a TCP connection
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
struct TransportLabels {
    /// Was the transport opened in the inbound or outbound direction?
    direction: Direction,

    peer: Peer,

    /// Was the transport secured with TLS?
    tls_status: TlsStatus,
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

/// Labels describing the end of a TCP connection
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
struct TransportCloseLabels {
    transport: TransportLabels,
    eos: Eos,
}

#[derive(Debug, Default)]
struct CloseMetrics {
    close_total: Counter,
    connection_duration: Histogram<latency::Ms>,
}

// ===== impl Transports =====

impl Transports {
    pub fn open(&mut self, ctx: &ctx::transport::Ctx) {
        let k = TransportLabels::new(ctx);
        let metrics = self.opens.get_or_default(k);
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
        let key = TransportLabels::new(ctx);

        let o = self.opens.get_or_default(key);
        o.open_connections.decr();
        o.read_bytes_total += rx;
        o.write_bytes_total += tx;

        let k = TransportCloseLabels::new(key, eos);
        let c = self.closes.get_or_default(k);
        c.close_total.incr();
        c.connection_duration.add(duration);
    }

    #[cfg(test)]
    pub fn open_total(&self, ctx: &ctx::transport::Ctx) -> u64 {
        self.opens
            .get(&TransportLabels::new(ctx))
            .map(|m| m.open_total.into())
            .unwrap_or(0)
    }

    // #[cfg(test)]
    // pub fn open_connections(&self, ctx: &ctx::transport::Ctx) -> u64 {
    //     self.metrics
    //         .get(&Key::from(ctx))
    //         .map(|m| m.open_connections.into())
    //         .unwrap_or(0)
    // }

    #[cfg(test)]
    pub fn rx_tx_bytes_total(&self, ctx: &ctx::transport::Ctx) -> (u64, u64) {
        self.opens
            .get(&TransportLabels::new(ctx))
            .map(|m| (m.read_bytes_total.into(), m.write_bytes_total.into()))
            .unwrap_or((0, 0))
    }

    #[cfg(test)]
    pub fn close_total(&self, ctx: &ctx::transport::Ctx, eos: Eos) -> u64 {
        self.closes
            .get(&TransportCloseLabels::new(TransportLabels::new(ctx), eos))
            .map(|m| m.close_total.into())
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub fn connection_durations(&self, ctx: &ctx::transport::Ctx, eos: Eos) -> Histogram<latency::Ms> {
        self.closes
            .get(&TransportCloseLabels::new(TransportLabels::new(ctx), eos))
            .map(|m| m.connection_duration.clone())
            .unwrap_or_default()
    }
}

impl fmt::Display for Transports {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.opens.fmt(f)?;
        self.closes.fmt(f)?;

        Ok(())
    }
}

// ===== impl OpenScopes =====

impl fmt::Display for OpenScopes {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.is_empty() {
            return Ok(());
        }

        tcp_open_total.fmt_help(f)?;
        tcp_open_total.fmt_scopes(f, self, |s| &s.open_total)?;

        tcp_open_connections.fmt_help(f)?;
        tcp_open_connections.fmt_scopes(f, self, |s| &s.open_connections)?;

        tcp_read_bytes_total.fmt_help(f)?;
        tcp_read_bytes_total.fmt_scopes(f, self, |s| &s.read_bytes_total)?;

        tcp_write_bytes_total.fmt_help(f)?;
        tcp_write_bytes_total.fmt_scopes(f, self, |s| &s.write_bytes_total)?;

        Ok(())
    }
}

// ===== impl CloseScopes =====

impl fmt::Display for CloseScopes {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.is_empty() {
            return Ok(());
        }

        tcp_close_total.fmt_help(f)?;
        tcp_close_total.fmt_scopes(f, self, |s| &s.close_total)?;

        tcp_connection_duration_ms.fmt_help(f)?;
        tcp_connection_duration_ms.fmt_scopes(f, self, |s| &s.connection_duration)?;

        Ok(())
    }
}

// ===== impl TransportLabels =====

impl TransportLabels {
    fn new(ctx: &ctx::transport::Ctx) -> Self {
        TransportLabels {
            direction: Direction::from_context(ctx.proxy().as_ref()),
            peer: match *ctx {
                ctx::transport::Ctx::Server(_) => Peer::Src,
                ctx::transport::Ctx::Client(_) => Peer::Dst,
            },
            tls_status: ctx.tls_status().into(),
        }
    }
}

impl fmt::Display for TransportLabels {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{},{},{}", self.direction, self.peer, self.tls_status)
    }
}

impl fmt::Display for Peer {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Peer::Src => f.pad("peer=\"src\""),
            Peer::Dst => f.pad("peer=\"dst\""),
        }
    }
}

// ===== impl TransportCloseLabels =====

impl TransportCloseLabels {
    fn new(transport: TransportLabels, eos: Eos) -> Self {
        Self {
            transport,
            eos,
        }
    }
}

impl fmt::Display for TransportCloseLabels {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{},{}", self.transport, self.eos)
    }
}

// ===== impl Eos =====

impl fmt::Display for Eos {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
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
