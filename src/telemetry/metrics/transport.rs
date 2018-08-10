use std::fmt;
use std::time::Duration;

use ctx;
use super::{
    labels::{Classification, Direction, TlsStatus},
    latency,
    Counter,
    Gauge,
    Histogram,
    Scopes,
};
use telemetry::{event, Errno};

pub(super) type OpenScopes = Scopes<TransportLabels, OpenMetrics>;

#[derive(Debug, Default)]
pub(super) struct OpenMetrics {
    open_total: Counter,
    open_connections: Gauge,
    write_bytes_total: Counter,
    read_bytes_total: Counter,
}

pub(super) type CloseScopes = Scopes<TransportCloseLabels, CloseMetrics>;

/// Labels describing a TCP connection
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct TransportLabels {
    /// Was the transport opened in the inbound or outbound direction?
    direction: Direction,

    peer: Peer,

    /// Was the transport secured with TLS?
    tls_status: TlsStatus,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Peer { Src, Dst }

/// Labels describing the end of a TCP connection
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct TransportCloseLabels {
    /// Labels describing the TCP connection that closed.
    pub(super) transport: TransportLabels,

    /// Was the transport closed successfully?
    classification: Classification,

    /// If `classification` == `Failure`, this may be set with the
    /// OS error number describing the error, if there was one.
    /// Otherwise, it should be `None`.
    errno: Option<Errno>,
}

#[derive(Debug, Default)]
pub(super) struct CloseMetrics {
    close_total: Counter,
    connection_duration: Histogram<latency::Ms>,
}

// ===== impl OpenScopes =====

impl OpenScopes {
    metrics! {
        tcp_open_total: Counter { "Total count of opened connections" },
        tcp_open_connections: Gauge { "Number of currently-open connections" },
        tcp_read_bytes_total: Counter { "Total count of bytes read from peers" },
        tcp_write_bytes_total: Counter { "Total count of bytes written to peers" }
    }
}

impl fmt::Display for OpenScopes {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.is_empty() {
            return Ok(());
        }

        Self::tcp_open_total.fmt_help(f)?;
        Self::tcp_open_total.fmt_scopes(f, self, |s| &s.open_total)?;

        Self::tcp_open_connections.fmt_help(f)?;
        Self::tcp_open_connections.fmt_scopes(f, self, |s| &s.open_connections)?;

        Self::tcp_read_bytes_total.fmt_help(f)?;
        Self::tcp_read_bytes_total.fmt_scopes(f, self, |s| &s.read_bytes_total)?;

        Self::tcp_write_bytes_total.fmt_help(f)?;
        Self::tcp_write_bytes_total.fmt_scopes(f, self, |s| &s.write_bytes_total)?;

        Ok(())
    }
}

// ===== impl OpenMetrics =====

impl OpenMetrics {
    pub(super) fn open(&mut self) {
        self.open_total.incr();
        self.open_connections.incr();
    }

    pub(super) fn close(&mut self, rx: u64, tx: u64) {
        self.open_connections.decr();
        self.read_bytes_total += rx;
        self.write_bytes_total += tx;
    }

    #[cfg(test)]
    pub(super) fn open_total(&self) -> u64 {
        self.open_total.into()
    }

    #[cfg(test)]
    pub(super) fn read_bytes_total(&self) -> u64 {
        self.read_bytes_total.into()
    }

    #[cfg(test)]
    pub(super) fn write_bytes_total(&self) -> u64 {
        self.write_bytes_total.into()
    }
}

// ===== impl CloseScopes =====

impl CloseScopes {
    metrics! {
        tcp_close_total: Counter { "Total count of closed connections" },
        tcp_connection_duration_ms: Histogram<latency::Ms> { "Connection lifetimes" }
    }
}

impl fmt::Display for CloseScopes {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.is_empty() {
            return Ok(());
        }

        Self::tcp_close_total.fmt_help(f)?;
        Self::tcp_close_total.fmt_scopes(f, self, |s| &s.close_total)?;

        Self::tcp_connection_duration_ms.fmt_help(f)?;
        Self::tcp_connection_duration_ms.fmt_scopes(f, self, |s| &s.connection_duration)?;

        Ok(())
    }
}

// ===== impl CloseMetrics =====

impl CloseMetrics {
    pub(super) fn close(&mut self, duration: Duration) {
        self.close_total.incr();
        self.connection_duration.add(duration);
    }

    #[cfg(test)]
    pub(super) fn close_total(&self) -> u64 {
        self.close_total.into()
    }

    #[cfg(test)]
    pub(super) fn connection_duration(&self) -> &Histogram<latency::Ms> {
        &self.connection_duration
    }
}


// ===== impl TransportLabels =====

impl TransportLabels {
    pub fn new(ctx: &ctx::transport::Ctx) -> Self {
        TransportLabels {
            direction: Direction::from_context(ctx.proxy().as_ref()),
            peer: match *ctx {
                ctx::transport::Ctx::Server(_) => Peer::Src,
                ctx::transport::Ctx::Client(_) => Peer::Dst,
            },
            tls_status: ctx.tls_status().into(),
        }
    }

    #[cfg(test)]
    pub fn tls_status(&self) -> TlsStatus {
        self.tls_status
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
    pub fn new(ctx: &ctx::transport::Ctx,
               close: &event::TransportClose)
               -> Self {
        let classification = Classification::transport_close(close);
        let errno = close.errno.map(|code| {
            // If the error code is set, this should be classified
            // as a failure!
            debug_assert!(classification == Classification::Failure);
            Errno::from(code)
        });
        TransportCloseLabels {
            transport: TransportLabels::new(ctx),
            classification,
            errno,
        }
    }

    #[cfg(test)]
    pub fn tls_status(&self) -> TlsStatus  {
        self.transport.tls_status()
    }
}

impl fmt::Display for TransportCloseLabels {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{},{}", self.transport, self.classification)?;
        if let Some(errno) = self.errno {
            write!(f, ",errno=\"{}\"", errno)?;
        }
        Ok(())
    }
}

