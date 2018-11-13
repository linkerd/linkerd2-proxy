use std::{
    fmt::{self, Write},
    net,
};

use metrics::FmtLabels;

use transport::tls;
use {Conditional, NameAddr};

use super::{classify, dst, inbound, outbound};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EndpointLabels {
    addr: net::SocketAddr,
    direction: Direction,
    tls_status: tls::Status,
    dst_name: Option<NameAddr>,
    labels: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RouteLabels {
    dst: dst::DstAddr,
    labels: Option<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
enum Direction {
    In,
    Out,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Authority<'a>(&'a NameAddr);

// === impl RouteLabels ===

impl From<dst::Route> for RouteLabels {
    fn from(r: dst::Route) -> Self {
        RouteLabels {
            dst: r.dst_addr,
            labels: prefix_labels("rt", r.route.labels().as_ref().into_iter()),
        }
    }
}

impl FmtLabels for RouteLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.dst.fmt_labels(f)?;

        if let Some(labels) = self.labels.as_ref() {
            write!(f, ",{}", labels)?;
        }

        Ok(())
    }
}

// === impl EndpointLabels ===

impl From<inbound::Endpoint> for EndpointLabels {
    fn from(ep: inbound::Endpoint) -> Self {
        Self {
            addr: ep.addr,
            dst_name: ep.dst_name,
            direction: Direction::In,
            tls_status: ep.source_tls_status,
            labels: None,
        }
    }
}

fn prefix_labels<'i, I>(prefix: &str, mut labels_iter: I) -> Option<String>
where
    I: Iterator<Item = (&'i String, &'i String)>,
{
    let (k0, v0) = labels_iter.next()?;
    let mut out = format!("{}_{}=\"{}\"", prefix, k0, v0);

    for (k, v) in labels_iter {
        write!(out, ",{}_{}=\"{}\"", prefix, k, v).expect("label concat must succeed");
    }
    Some(out)
}

impl From<outbound::Endpoint> for EndpointLabels {
    fn from(ep: outbound::Endpoint) -> Self {
        Self {
            addr: ep.connect.addr,
            dst_name: ep.dst_name,
            direction: Direction::Out,
            tls_status: ep.connect.tls_status(),
            labels: prefix_labels("dst", ep.metadata.labels().into_iter()),
        }
    }
}

impl FmtLabels for EndpointLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let authority = self.dst_name.as_ref().map(Authority);
        (authority, &self.direction).fmt_labels(f)?;

        if let Some(labels) = self.labels.as_ref() {
            write!(f, ",{}", labels)?;
        }

        write!(f, ",")?;
        self.tls_status.fmt_labels(f)?;

        Ok(())
    }
}

impl FmtLabels for Direction {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Direction::In => write!(f, "direction=\"inbound\""),
            Direction::Out => write!(f, "direction=\"outbound\""),
        }
    }
}

impl<'a> FmtLabels for Authority<'a> {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.0.port() == 80 {
            write!(f, "authority=\"{}\"", self.0.name().without_trailing_dot())
        } else {
            write!(f, "authority=\"{}\"", self.0)
        }
    }
}

impl FmtLabels for dst::DstAddr {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.direction() {
            dst::Direction::In => Direction::In.fmt_labels(f)?,
            dst::Direction::Out => Direction::Out.fmt_labels(f)?,
        }

        write!(f, ",dst=\"{}\"", self.as_ref())
    }
}

impl FmtLabels for classify::Class {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::classify::Class;
        match self {
            Class::Default(result) => write!(f, "classification=\"{}\"", result),
            Class::Grpc(result, status) => write!(
                f,
                "classification=\"{}\",grpc_status=\"{}\"",
                result, status
            ),
            Class::Stream(result, status) => {
                write!(f, "classification=\"{}\",h2_err=\"{}\"", result, status)
            }
        }
    }
}

impl fmt::Display for classify::SuccessOrFailure {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::classify::SuccessOrFailure::{Failure, Success};
        match self {
            Success => write!(f, "success"),
            Failure => write!(f, "failure"),
        }
    }
}

impl FmtLabels for tls::Status {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Conditional::None(tls::ReasonForNoTls::NoIdentity(why)) => {
                write!(f, "tls=\"no_identity\",no_tls_reason=\"{}\"", why)
            }
            status => write!(f, "tls=\"{}\"", status),
        }
    }
}
