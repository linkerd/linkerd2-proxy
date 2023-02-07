use crate::{http, tcp};
use linkerd_app_core::{
    metrics,
    profiles::LogicalAddr,
    proxy::api_resolve::Metadata,
    svc, tls,
    transport::{self, addrs::*},
    transport_header, Conditional, Error,
};
use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint<P> {
    pub addr: Remote<ServerAddr>,
    pub tls: tls::ConditionalClientTls,
    pub metadata: Metadata,
    pub logical_addr: Option<LogicalAddr>,
    pub protocol: P,
    pub opaque_protocol: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("forwarding to {addr}: {source}")]
pub struct ForwardError {
    addr: Remote<ServerAddr>,
    #[source]
    source: Error,
}

// === impl Endpoint ===

impl Endpoint<()> {
    pub(crate) fn forward(
        addr: OrigDstAddr,
        reason: tls::NoClientTls,
        opaque_protocol: bool,
    ) -> Self {
        Self {
            addr: Remote(ServerAddr(addr.into())),
            metadata: Metadata::default(),
            tls: Conditional::None(reason),
            logical_addr: None,
            opaque_protocol,
            protocol: (),
        }
    }

    pub fn from_metadata(
        addr: impl Into<SocketAddr>,
        mut metadata: Metadata,
        reason: tls::NoClientTls,
        opaque_protocol: bool,
        inbound_ips: &HashSet<IpAddr>,
    ) -> Self {
        let addr: SocketAddr = addr.into();
        let tls = if inbound_ips.contains(&addr.ip()) {
            #[allow(deprecated)]
            metadata.clear_upgrade();
            tracing::debug!(%addr, ?metadata, ?addr, ?inbound_ips, "Target is local");
            tls::ConditionalClientTls::None(tls::NoClientTls::Loopback)
        } else {
            client_tls(&metadata, reason)
        };

        Self {
            addr: Remote(ServerAddr(addr)),
            tls,
            metadata,
            logical_addr: None,
            opaque_protocol,
            protocol: (),
        }
    }
}

impl<P> svc::Param<Remote<ServerAddr>> for Endpoint<P> {
    fn param(&self) -> Remote<ServerAddr> {
        self.addr
    }
}

impl<P> svc::Param<tls::ConditionalClientTls> for Endpoint<P> {
    fn param(&self) -> tls::ConditionalClientTls {
        self.tls.clone()
    }
}

impl<P> svc::Param<Option<http::detect::Skip>> for Endpoint<P> {
    fn param(&self) -> Option<http::detect::Skip> {
        if self.opaque_protocol {
            Some(http::detect::Skip)
        } else {
            None
        }
    }
}

impl<P> svc::Param<Option<tcp::opaque_transport::PortOverride>> for Endpoint<P> {
    fn param(&self) -> Option<tcp::opaque_transport::PortOverride> {
        self.metadata
            .opaque_transport_port()
            .map(tcp::opaque_transport::PortOverride)
    }
}

impl<P> svc::Param<Option<http::AuthorityOverride>> for Endpoint<P> {
    fn param(&self) -> Option<http::AuthorityOverride> {
        self.metadata
            .authority_override()
            .cloned()
            .map(http::AuthorityOverride)
    }
}

impl<P> svc::Param<transport::labels::Key> for Endpoint<P> {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::OutboundClient(self.param())
    }
}

impl<P> svc::Param<metrics::OutboundEndpointLabels> for Endpoint<P> {
    fn param(&self) -> metrics::OutboundEndpointLabels {
        let authority = self
            .logical_addr
            .as_ref()
            .map(|LogicalAddr(a)| a.as_http_authority());
        metrics::OutboundEndpointLabels {
            authority,
            labels: metrics::prefix_labels("dst", self.metadata.labels().iter()),
            server_id: self.tls.clone(),
            target_addr: self.addr.into(),
        }
    }
}

impl<P> svc::Param<metrics::EndpointLabels> for Endpoint<P> {
    fn param(&self) -> metrics::EndpointLabels {
        svc::Param::<metrics::OutboundEndpointLabels>::param(self).into()
    }
}

// === NewFromMetadata ===

fn client_tls(metadata: &Metadata, reason: tls::NoClientTls) -> tls::ConditionalClientTls {
    // If we're transporting an opaque protocol OR we're communicating with
    // a gateway, then set an ALPN value indicating support for a transport
    // header.
    let use_transport_header =
        metadata.opaque_transport_port().is_some() || metadata.authority_override().is_some();

    metadata
        .identity()
        .cloned()
        .map(move |server_id| {
            Conditional::Some(tls::ClientTls {
                server_id,
                alpn: if use_transport_header {
                    Some(tls::client::AlpnProtocols(vec![
                        transport_header::PROTOCOL.into()
                    ]))
                } else {
                    None
                },
            })
        })
        .unwrap_or(Conditional::None(reason))
}

// === impl ForwardError ===

impl<T> From<(&T, Error)> for ForwardError
where
    T: svc::Param<Remote<ServerAddr>>,
{
    fn from((target, source): (&T, Error)) -> Self {
        Self {
            addr: target.param(),
            source,
        }
    }
}
