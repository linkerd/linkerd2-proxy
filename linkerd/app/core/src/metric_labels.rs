use crate::identity;
use crate::transport::{labels::TlsStatus, tls};
use linkerd2_addr::{Addr, NameAddr};
use linkerd2_conditional::Conditional;
use linkerd2_metrics::FmtLabels;
use std::fmt::{self, Write};

use super::{classify, control, dst};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ControlLabels {
    addr: Addr,
    tls_status: TlsStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EndpointLabels {
    pub direction: Direction,
    pub tls_id: Conditional<TlsId, tls::ReasonForNoIdentity>,
    pub dst_logical: Option<NameAddr>,
    pub dst_concrete: Option<NameAddr>,
    pub labels: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RouteLabels {
    dst: dst::DstAddr,
    labels: Option<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    In,
    Out,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TlsId {
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

impl FmtLabels for EndpointLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let authority = self.dst_logical.as_ref().map(Authority);
        (authority, &self.direction).fmt_labels(f)?;

        if let Some(labels) = self.labels.as_ref() {
            write!(f, ",{}", labels)?;
        }

        write!(f, ",")?;
        let tls_id = self.tls_id.clone().map(Into::<identity::Name>::into);
        TlsStatus::from(tls_id.as_ref()).fmt_labels(f)?;

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

impl Into<identity::Name> for TlsId {
    fn into(self) -> identity::Name {
        match self {
            TlsId::ClientId(name) => name,
            TlsId::ServerId(name) => name,
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

impl FmtLabels for TlsId {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TlsId::ClientId(ref id) => write!(f, "client_id=\"{}\"", id.as_ref()),
            TlsId::ServerId(ref id) => write!(f, "server_id=\"{}\"", id.as_ref()),
        }
    }
}

pub fn prefix_labels<'i, I>(prefix: &str, mut labels_iter: I) -> Option<String>
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
