pub use crate::metrics::{Direction, OutboundEndpointLabels, ServerLabel as PolicyServerLabel};
use linkerd_conditional::Conditional;
use linkerd_metrics::FmtLabels;
use linkerd_tls as tls;
use std::{fmt, net::SocketAddr};

/// Describes a class of transport.
///
/// A `Metrics` type exists for each unique `Key`.
///
/// Implements `FmtLabels`.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum Key {
    Server(ServerLabels),
    OutboundClient(OutboundEndpointLabels),
    InboundClient,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ServerLabels {
    direction: Direction,
    tls: tls::ConditionalServerTls,
    target_addr: SocketAddr,
    policy: Option<PolicyServerLabel>,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct TlsAccept<'t>(pub &'t tls::ConditionalServerTls);

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct TlsConnect<'t>(&'t tls::ConditionalClientTls);

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct TargetAddr(pub SocketAddr);

// === impl Key ===

impl Key {
    pub fn inbound_server(
        tls: tls::ConditionalServerTls,
        target_addr: SocketAddr,
        server: PolicyServerLabel,
    ) -> Self {
        Self::Server(ServerLabels::inbound(tls, target_addr, server))
    }

    pub fn outbound_server(target_addr: SocketAddr) -> Self {
        Self::Server(ServerLabels::outbound(target_addr))
    }
}

impl FmtLabels for Key {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Server(l) => l.fmt_labels(f),

            Self::OutboundClient(endpoint) => {
                Direction::Out.fmt_labels(f)?;
                write!(f, ",peer=\"dst\",")?;
                endpoint.fmt_labels(f)
            }

            Self::InboundClient => {
                const NO_TLS: tls::client::ConditionalClientTls =
                    Conditional::None(tls::NoClientTls::Loopback);

                Direction::In.fmt_labels(f)?;
                write!(f, ",peer=\"dst\",")?;
                TlsConnect(&NO_TLS).fmt_labels(f)
            }
        }
    }
}

impl ServerLabels {
    fn inbound(
        tls: tls::ConditionalServerTls,
        target_addr: SocketAddr,
        policy: PolicyServerLabel,
    ) -> Self {
        ServerLabels {
            direction: Direction::In,
            tls,
            target_addr,
            policy: Some(policy),
        }
    }

    fn outbound(target_addr: SocketAddr) -> Self {
        ServerLabels {
            direction: Direction::Out,
            tls: tls::ConditionalServerTls::None(tls::NoServerTls::Loopback),
            target_addr,
            policy: None,
        }
    }
}

impl FmtLabels for ServerLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.direction.fmt_labels(f)?;
        f.write_str(",peer=\"src\",")?;
        (
            (TargetAddr(self.target_addr), TlsAccept(&self.tls)),
            self.policy.as_ref(),
        )
            .fmt_labels(f)?;

        Ok(())
    }
}

// === impl TlsAccept ===

impl<'t> From<&'t tls::ConditionalServerTls> for TlsAccept<'t> {
    fn from(c: &'t tls::ConditionalServerTls) -> Self {
        TlsAccept(c)
    }
}

impl<'t> FmtLabels for TlsAccept<'t> {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Conditional::None(tls::NoServerTls::Disabled) => {
                write!(f, "tls=\"disabled\"")
            }
            Conditional::None(why) => {
                write!(f, "tls=\"no_identity\",no_tls_reason=\"{}\"", why)
            }
            Conditional::Some(tls::ServerTls::Established { client_id, .. }) => match client_id {
                Some(id) => write!(f, "tls=\"true\",client_id=\"{}\"", id),
                None => write!(f, "tls=\"true\",client_id=\"\""),
            },
            Conditional::Some(tls::ServerTls::Passthru { sni }) => {
                write!(f, "tls=\"opaque\",sni=\"{}\"", sni)
            }
        }
    }
}

// === impl TlsConnect ===

impl<'t> From<&'t tls::ConditionalClientTls> for TlsConnect<'t> {
    fn from(s: &'t tls::ConditionalClientTls) -> Self {
        TlsConnect(s)
    }
}

impl<'t> FmtLabels for TlsConnect<'t> {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Conditional::None(tls::NoClientTls::Disabled) => {
                write!(f, "tls=\"disabled\"")
            }
            Conditional::None(why) => {
                write!(f, "tls=\"no_identity\",no_tls_reason=\"{}\"", why)
            }
            Conditional::Some(tls::ClientTls { server_id, .. }) => {
                write!(f, "tls=\"true\",server_id=\"{}\"", server_id)
            }
        }
    }
}

// === impl TargetAddr ===

impl FmtLabels for TargetAddr {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "target_addr=\"{}\",target_ip=\"{}\",target_port=\"{}\"",
            self.0,
            self.0.ip(),
            self.0.port()
        )
    }
}

#[cfg(test)]
mod tests {
    pub use super::*;

    impl std::fmt::Display for ServerLabels {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.fmt_labels(f)
        }
    }

    #[test]
    fn server_labels() {
        let labels = ServerLabels::inbound(
            tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                client_id: Some("foo.id.example.com".parse().unwrap()),
                negotiated_protocol: None,
            }),
            ([192, 0, 2, 4], 40000).into(),
            PolicyServerLabel {
                kind: "server".into(),
                name: "testserver".into(),
            },
        );
        assert_eq!(
            labels.to_string(),
            "direction=\"inbound\",peer=\"src\",\
            target_addr=\"192.0.2.4:40000\",target_ip=\"192.0.2.4\",target_port=\"40000\",\
            tls=\"true\",client_id=\"foo.id.example.com\",\
            srv_kind=\"server\",srv_name=\"testserver\""
        );
    }
}
