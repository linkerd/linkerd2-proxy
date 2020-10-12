use crate::proxy::identity;
pub use crate::transport::labels::TlsStatus;
use linkerd2_addr::Addr;
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
    pub tls_id: TlsStatus,
    pub authority: Option<http::uri::Authority>,
    pub labels: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StackLabels {
    pub direction: Direction,
    pub name: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RouteLabels {
    direction: Direction,
    target: Addr,
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
struct Authority<'a>(&'a http::uri::Authority);

// === impl CtlLabels ===

impl From<&'_ control::ControlAddr> for ControlLabels {
    fn from(c: &'_ control::ControlAddr) -> Self {
        ControlLabels {
            addr: c.addr.clone(),
            tls_status: c.identity.clone().map(|id| TlsId::ServerId(id)).into(),
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

impl From<&'_ dst::Route> for RouteLabels {
    fn from(r: &'_ dst::Route) -> Self {
        Self {
            target: r.target.clone(),
            direction: r.direction,
            labels: prefix_labels("rt", r.route.labels().iter()),
        }
    }
}

impl FmtLabels for RouteLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.direction.fmt_labels(f)?;
        write!(f, ",dst=\"{}\"", self.target)?;

        if let Some(labels) = self.labels.as_ref() {
            write!(f, ",{}", labels)?;
        }

        Ok(())
    }
}

// === impl EndpointLabels ===

impl FmtLabels for EndpointLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let authority = self.authority.as_ref().map(Authority);
        (authority, &self.direction).fmt_labels(f)?;

        if let Some(labels) = self.labels.as_ref() {
            write!(f, ",{}", labels)?;
        }

        write!(f, ",")?;
        self.tls_id.fmt_labels(f)?;

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
        write!(f, "authority=\"{}\"", self.0)
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

// === impl StackLabels ===

impl StackLabels {
    pub fn inbound(name: &'static str) -> Self {
        Self {
            direction: Direction::In,
            name,
        }
    }

    pub fn outbound(name: &'static str) -> Self {
        Self {
            direction: Direction::Out,
            name,
        }
    }
}

impl FmtLabels for StackLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.direction.fmt_labels(f)?;
        write!(f, ",name=\"{}\"", self.name)
    }
}
