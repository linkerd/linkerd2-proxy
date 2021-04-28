use crate::{
    logical::{Concrete, Logical},
    tcp::opaque_transport,
    Accept, Outbound,
};
use linkerd_app_core::{
    metrics,
    profiles::LogicalAddr,
    proxy::{api_resolve::Metadata, http, resolve::map_endpoint::MapEndpoint},
    svc::{self, Param},
    tls,
    transport::{self, Remote, ServerAddr},
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
}

#[derive(Copy, Clone)]
pub struct FromMetadata {
    pub identity_disabled: bool,
}

impl<E> Outbound<E> {
    pub fn push_into_endpoint<P, T>(
        self,
    ) -> Outbound<impl svc::NewService<T, Service = E::Service> + Clone>
    where
        Endpoint<P>: From<(tls::NoClientTls, T)>,
        E: svc::NewService<Endpoint<P>> + Clone,
    {
        let Self {
            config,
            runtime,
            stack: endpoint,
        } = self;
        let identity_disabled = runtime.identity.is_none();
        let no_tls_reason = if identity_disabled {
            tls::NoClientTls::Disabled
        } else {
            tls::NoClientTls::NotProvidedByServiceDiscovery
        };
        let stack =
            svc::stack(endpoint).push_map_target(move |t| Endpoint::<P>::from((no_tls_reason, t)));
        Outbound {
            config,
            runtime,
            stack,
        }
    }
}

// === impl Endpoint ===

impl<P> Endpoint<P> {
    pub fn no_tls(reason: tls::NoClientTls) -> impl Fn(Accept<P>) -> Self {
        move |accept| Self::from((reason, accept))
    }
}

impl<P> From<(tls::NoClientTls, Logical<P>)> for Endpoint<P> {
    fn from((reason, logical): (tls::NoClientTls, Logical<P>)) -> Self {
        match logical.profile.borrow().endpoint.clone() {
            None => Self {
                addr: Remote(ServerAddr(logical.orig_dst.into())),
                metadata: Metadata::default(),
                tls: Conditional::None(reason),
                logical_addr: Some(logical.logical_addr),
                protocol: logical.protocol,
            },
            Some((addr, metadata)) => Self {
                addr: Remote(ServerAddr(addr)),
                tls: FromMetadata::client_tls(&metadata, reason),
                metadata,
                logical_addr: Some(logical.logical_addr),
                protocol: logical.protocol,
            },
        }
    }
}

impl<P> From<(tls::NoClientTls, Accept<P>)> for Endpoint<P> {
    fn from((reason, accept): (tls::NoClientTls, Accept<P>)) -> Self {
        Self {
            addr: Remote(ServerAddr(accept.orig_dst.into())),
            metadata: Metadata::default(),
            tls: Conditional::None(reason),
            logical_addr: None,
            protocol: accept.protocol,
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
        }
    }
}
