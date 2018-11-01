use http::uri;
use std::{
    fmt::{self, Write},
    net,
};

use metrics::FmtLabels;

use transport::tls;
use Conditional;

use super::{classify, inbound, outbound};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EndpointLabels {
    addr: net::SocketAddr,
    direction: Direction,
    tls_status: tls::Status,
    authority: Authority,
    labels: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RouteLabels {
    direction: Direction,
    dst: Dst,
    labels: Option<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
enum Direction {
    In,
    Out,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Authority(Option<uri::Authority>);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Dst(outbound::Destination);

// === impl RouteLabels ===

impl From<outbound::Route> for RouteLabels {
    fn from(r: outbound::Route) -> Self {
        let mut label_iter = r.route.labels().as_ref().into_iter();
        let labels = if let Some((k0, v0)) = label_iter.next() {
            let mut s = format!("rt_{}=\"{}\"", k0, v0);
            for (k, v) in label_iter {
                write!(s, ",rt_{}=\"{}\"", k, v).expect("label concat must succeed");
            }
            Some(s)
        } else {
            None
        };

        RouteLabels {
            dst: Dst(r.dst.clone()),
            direction: Direction::Out,
            labels,
        }
    }
}

impl FmtLabels for RouteLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        (&self.dst, &self.direction).fmt_labels(f)?;

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
            authority: Authority(Some(ep.authority)),
            direction: Direction::In,
            tls_status: ep.source_tls_status,
            labels: None,
        }
    }
}

impl From<outbound::Endpoint> for EndpointLabels {
    fn from(ep: outbound::Endpoint) -> Self {
        use self::outbound::NameOrAddr;
        use transport::DnsNameAndPort;

        let mut label_iter = ep.metadata.labels().into_iter();
        let labels = if let Some((k0, v0)) = label_iter.next() {
            let mut s = format!("dst_{}=\"{}\"", k0, v0);
            for (k, v) in label_iter {
                write!(s, ",dst_{}=\"{}\"", k, v).expect("label concat must succeed");
            }
            Some(s)
        } else {
            None
        };

        let authority = {
            let a = match ep.dst.name_or_addr {
                NameOrAddr::Name(DnsNameAndPort { ref host, ref port }) => {
                    if *port == 80 {
                        format!("{}", host)
                    } else {
                        format!("{}:{}", host, port)
                    }
                }
                NameOrAddr::Addr(addr) => format!("{}", addr),
            };
            Authority(uri::Authority::from_shared(a.into()).ok())
        };

        Self {
            addr: ep.connect.addr,
            authority,
            direction: Direction::Out,
            tls_status: ep.connect.tls_status(),
            labels,
        }
    }
}

impl FmtLabels for EndpointLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        (&self.authority, &self.direction).fmt_labels(f)?;

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

impl FmtLabels for Authority {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0 {
            Some(ref a) => write!(f, "authority=\"{}\"", a),
            None => write!(f, "authority=\"\""),
        }
    }
}

impl FmtLabels for Dst {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "dst=\"{}\"", self.0.name_or_addr)?;
        write!(
            f,
            ",dst_protocol=\"{}\"",
            if self.0.settings.is_http2() {
                "h2"
            } else {
                "h1"
            }
        )?;

        Ok(())
    }
}

impl FmtLabels for classify::Class {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::classify::Class;
        match self {
            Class::Grpc(result, status) => write!(
                f,
                "classification=\"{}\",grpc_status=\"{}\"",
                result, status
            ),
            Class::Http(result) => write!(f, "classification=\"{}\"", result),
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
