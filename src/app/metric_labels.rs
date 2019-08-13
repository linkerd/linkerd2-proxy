use crate::identity;
use crate::transport::tls;
use crate::{Addr, Conditional, NameAddr};
use linkerd2_metrics::FmtLabels;
use std::fmt::{self, Write};

use super::{classify, control, dst, inbound, outbound};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ControlLabels {
    addr: Addr,
    tls_status: tls::Status,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EndpointLabels {
    direction: Direction,
    tls_id: Conditional<TlsId, tls::ReasonForNoIdentity>,
    dst_logical: Option<NameAddr>,
    dst_concrete: Option<NameAddr>,
    labels: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RouteLabels {
    dst: dst::DstAddr,
    labels: Option<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(in crate::app) enum Direction {
    In,
    Out,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum TlsId {
    ClientId(identity::Name),
    ServerId(identity::Name),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Authority<'a>(&'a NameAddr);

// === impl CtlLabels ===

impl From<control::ControlAddr> for ControlLabels {
    fn from(c: control::ControlAddr) -> Self {
        ControlLabels {
            addr: c.addr.clone(),
            tls_status: c.identity.as_ref().into(),
        }
    }
}

impl FmtLabels for ControlLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "addr=\"{}\",", self.addr)?;
        self.tls_status.fmt_labels(f)?;

        Ok(())
    }
}

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
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
            dst_logical: ep.dst_name.clone(),
            dst_concrete: ep.dst_name,
            direction: Direction::In,
            tls_id: ep.tls_client_id.map(TlsId::ClientId),
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
            dst_logical: ep.dst_logical,
            dst_concrete: ep.dst_concrete,
            direction: Direction::Out,
            tls_id: ep.identity.as_ref().map(|id| TlsId::ServerId(id.clone())),
            labels: prefix_labels("dst", ep.metadata.labels().into_iter()),
        }
    }
}

impl FmtLabels for EndpointLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let authority = self.dst_logical.as_ref().map(Authority);
        (authority, &self.direction).fmt_labels(f)?;

        if let Some(labels) = self.labels.as_ref() {
            write!(f, ",{}", labels)?;
        }

        write!(f, ",")?;
        tls::Status::from(self.tls_id.as_ref()).fmt_labels(f)?;

        if let Conditional::Some(ref id) = self.tls_id {
            write!(f, ",")?;
            id.fmt_labels(f)?;
        }

        Ok(())
    }
}

impl FmtLabels for Direction {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Direction::In => write!(f, "direction=\"inbound\""),
            Direction::Out => write!(f, "direction=\"outbound\""),
        }
    }
}

impl<'a> FmtLabels for Authority<'a> {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.port() == 80 {
            write!(f, "authority=\"{}\"", self.0.name().without_trailing_dot())
        } else {
            write!(f, "authority=\"{}\"", self.0)
        }
    }
}

impl FmtLabels for dst::DstAddr {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.direction() {
            dst::Direction::In => Direction::In.fmt_labels(f)?,
            dst::Direction::Out => Direction::Out.fmt_labels(f)?,
        }

        write!(f, ",dst=\"{}\"", self.as_ref())
    }
}

impl FmtLabels for classify::Class {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use self::classify::Class;
        match self {
            Class::Default(result) => write!(f, "classification=\"{}\"", result),
            Class::Grpc(result, status) => write!(
                f,
                "classification=\"{}\",grpc_status=\"{}\"",
                result, status
            ),
            Class::Stream(result, status) => {
                write!(f, "classification=\"{}\",error=\"{}\"", result, status)
            }
        }
    }
}

impl fmt::Display for classify::SuccessOrFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            classify::SuccessOrFailure::Success => write!(f, "success"),
            classify::SuccessOrFailure::Failure => write!(f, "failure"),
        }
    }
}

impl FmtLabels for tls::Status {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(tls::ReasonForNoIdentity::NoPeerName(why)) = self.no_tls_reason() {
            return write!(f, "tls=\"no_identity\",no_tls_reason=\"{}\"", why);
        }

        write!(f, "tls=\"{}\"", self)
    }
}

impl FmtLabels for TlsId {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TlsId::ClientId(ref id) => write!(f, "client_id=\"{}\"", id.as_ref()),
            TlsId::ServerId(ref id) => write!(f, "server_id=\"{}\"", id.as_ref()),
        }
    }
}
