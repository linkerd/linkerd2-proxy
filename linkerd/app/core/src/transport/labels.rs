use crate::metrics::ServerLabel as PolicyServerLabel;
pub use crate::metrics::{Direction, OutboundEndpointLabels};
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
    tls: tls::ConditionalServerTlsLabels,
    target_addr: SocketAddr,
    policy: Option<PolicyServerLabel>,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct TlsAccept<'t>(pub &'t tls::ConditionalServerTlsLabels);

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct TlsConnect<'t>(pub &'t tls::ConditionalClientTlsLabels);

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct TargetAddr(pub SocketAddr);

// === impl Key ===

impl Key {
    pub fn inbound_server(
        tls: tls::ConditionalServerTlsLabels,
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
                const NO_TLS: tls::client::ConditionalClientTlsLabels =
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
        tls: tls::ConditionalServerTlsLabels,
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
            tls: tls::ConditionalServerTlsLabels::None(tls::NoServerTls::Loopback),
            target_addr,
            policy: None,
        }
    }
}

impl FmtLabels for ServerLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            direction,
            tls,
            target_addr,
            policy,
        } = self;

        direction.fmt_labels(f)?;
        f.write_str(",peer=\"src\",")?;

        ((TargetAddr(*target_addr), TlsAccept(tls)), policy.as_ref()).fmt_labels(f)?;

        Ok(())
    }
}

// === impl TlsAccept ===

impl<'t> From<&'t tls::ConditionalServerTlsLabels> for TlsAccept<'t> {
    fn from(c: &'t tls::ConditionalServerTlsLabels) -> Self {
        TlsAccept(c)
    }
}

impl FmtLabels for TlsAccept<'_> {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Conditional::None(tls::NoServerTls::Disabled) => {
                write!(f, "tls=\"disabled\"")
            }
            Conditional::None(why) => {
                write!(f, "tls=\"no_identity\",no_tls_reason=\"{}\"", why)
            }
            Conditional::Some(tls::ServerTlsLabels::Established { client_id }) => match client_id {
                Some(id) => write!(f, "tls=\"true\",client_id=\"{}\"", id),
                None => write!(f, "tls=\"true\",client_id=\"\""),
            },
            Conditional::Some(tls::ServerTlsLabels::Passthru { sni }) => {
                write!(f, "tls=\"opaque\",sni=\"{}\"", sni)
            }
        }
    }
}

// === impl TlsConnect ===

impl<'t> From<&'t tls::ConditionalClientTlsLabels> for TlsConnect<'t> {
    fn from(s: &'t tls::ConditionalClientTlsLabels) -> Self {
        TlsConnect(s)
    }
}

impl FmtLabels for TlsConnect<'_> {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self(tls) = self;

        match tls {
            Conditional::None(tls::NoClientTls::Disabled) => {
                write!(f, "tls=\"disabled\"")
            }
            Conditional::None(why) => {
                write!(f, "tls=\"no_identity\",no_tls_reason=\"{}\"", why)
            }
            Conditional::Some(tls::ClientTlsLabels { server_id }) => {
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
        use linkerd_proxy_server_policy::Meta;
        use std::sync::Arc;

        let labels = ServerLabels::inbound(
            tls::ConditionalServerTlsLabels::Some(tls::ServerTlsLabels::Established {
                client_id: Some("foo.id.example.com".parse().unwrap()),
            }),
            ([192, 0, 2, 4], 40000).into(),
            PolicyServerLabel(
                Arc::new(Meta::Resource {
                    group: "policy.linkerd.io".into(),
                    kind: "server".into(),
                    name: "testserver".into(),
                }),
                40000,
            ),
        );
        assert_eq!(
            labels.to_string(),
            "direction=\"inbound\",peer=\"src\",\
            target_addr=\"192.0.2.4:40000\",target_ip=\"192.0.2.4\",target_port=\"40000\",\
            tls=\"true\",client_id=\"foo.id.example.com\",\
            srv_group=\"policy.linkerd.io\",srv_kind=\"server\",srv_name=\"testserver\",srv_port=\"40000\""
        );
    }
}
