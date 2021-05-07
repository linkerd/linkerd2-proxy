use crate::{http, logical::Concrete, tcp::opaque_transport};
use linkerd_app_core::{
    metrics,
    profiles::LogicalAddr,
    proxy::{api_resolve::Metadata, resolve::map_endpoint::MapEndpoint},
    svc::Param,
    tls,
    transport::{self, addrs::*},
    transport_header, Conditional,
};
use std::net::SocketAddr;

#[derive(Clone, Debug)]
pub struct Endpoint<P> {
    pub addr: Remote<ServerAddr>,
    pub tls: tls::ConditionalClientTls,
    pub metadata: Metadata,
    pub logical_addr: Option<LogicalAddr>,
    pub protocol: P,
    pub opaque_protocol: bool,
}

#[derive(Copy, Clone)]
pub struct FromMetadata {
    pub identity_disabled: bool,
}

// === impl Endpoint ===

impl Endpoint<()> {
    pub(crate) fn forward(addr: OrigDstAddr, reason: tls::NoClientTls) -> Self {
        Self {
            addr: Remote(ServerAddr(addr.into())),
            metadata: Metadata::default(),
            tls: Conditional::None(reason),
            logical_addr: None,
            opaque_protocol: false,
            protocol: (),
        }
    }

    pub(crate) fn from_metadata(
        addr: SocketAddr,
        metadata: Metadata,
        reason: tls::NoClientTls,
        opaque_protocol: bool,
    ) -> Self {
        Self {
            addr: Remote(ServerAddr(addr)),
            tls: FromMetadata::client_tls(&metadata, reason),
            metadata,
            logical_addr: None,
            opaque_protocol,
            protocol: (),
        }
    }
}

impl<P> Param<Remote<ServerAddr>> for Endpoint<P> {
    fn param(&self) -> Remote<ServerAddr> {
        self.addr
    }
}

impl<P> Param<tls::ConditionalClientTls> for Endpoint<P> {
    fn param(&self) -> tls::ConditionalClientTls {
        self.tls.clone()
    }
}

impl<P> Param<Option<http::detect::Skip>> for Endpoint<P> {
    fn param(&self) -> Option<http::detect::Skip> {
        if self.opaque_protocol {
            Some(http::detect::Skip)
        } else {
            None
        }
    }
}

impl<P> Param<Option<opaque_transport::PortOverride>> for Endpoint<P> {
    fn param(&self) -> Option<opaque_transport::PortOverride> {
        self.metadata
            .opaque_transport_port()
            .map(opaque_transport::PortOverride)
    }
}

impl<P> Param<Option<http::AuthorityOverride>> for Endpoint<P> {
    fn param(&self) -> Option<http::AuthorityOverride> {
        self.metadata
            .authority_override()
            .cloned()
            .map(http::AuthorityOverride)
    }
}

impl<P> Param<transport::labels::Key> for Endpoint<P> {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::OutboundConnect(self.param())
    }
}

impl<P> Param<metrics::OutboundEndpointLabels> for Endpoint<P> {
    fn param(&self) -> metrics::OutboundEndpointLabels {
        let target_addr = self.addr.into();
        let authority = self
            .logical_addr
            .as_ref()
            .map(|LogicalAddr(a)| a.as_http_authority());
        metrics::OutboundEndpointLabels {
            authority,
            labels: metrics::prefix_labels("dst", self.metadata.labels().iter()),
            server_id: self.tls.clone(),
            target_addr,
        }
    }
}

impl<P> Param<metrics::EndpointLabels> for Endpoint<P> {
    fn param(&self) -> metrics::EndpointLabels {
        Param::<metrics::OutboundEndpointLabels>::param(self).into()
    }
}

impl<P: std::hash::Hash> std::hash::Hash for Endpoint<P> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.addr.hash(state);
        self.tls.hash(state);
        self.logical_addr.hash(state);
        self.protocol.hash(state);
    }
}

// === EndpointFromMetadata ===

impl FromMetadata {
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
}

impl<P: Copy + std::fmt::Debug> MapEndpoint<Concrete<P>, Metadata> for FromMetadata {
    type Out = Endpoint<P>;

    fn map_endpoint(
        &self,
        concrete: &Concrete<P>,
        addr: SocketAddr,
        metadata: Metadata,
    ) -> Self::Out {
        tracing::trace!(%addr, ?metadata, ?concrete, "Resolved endpoint");
        let tls = if self.identity_disabled {
            tls::ConditionalClientTls::None(tls::NoClientTls::Disabled)
        } else {
            Self::client_tls(&metadata, tls::NoClientTls::NotProvidedByServiceDiscovery)
        };
        Endpoint {
            addr: Remote(ServerAddr(addr)),
            tls,
            metadata,
            logical_addr: Some(concrete.logical.logical_addr.clone()),
            protocol: concrete.logical.protocol,
            // XXX We never do protocol detection after resolving a concrete address to endpoints.
            // We should differentiate these target types statically.
            opaque_protocol: false,
        }
    }
}
