use linkerd_app_core::{
    metrics, profiles,
    proxy::{api_resolve::Metadata, resolve::map_endpoint::MapEndpoint},
    svc::stack::Param,
    tls, transport, transport_header, Addr, Conditional,
};
use std::net::SocketAddr;

#[derive(Copy, Clone)]
pub struct EndpointFromMetadata;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Accept<P> {
    pub orig_dst: SocketAddr,
    pub protocol: P,
}

#[derive(Clone)]
pub struct Logical<P> {
    pub orig_dst: SocketAddr,
    pub profile: Option<profiles::Receiver>,
    pub protocol: P,
}

#[derive(Clone, Debug)]
pub struct Concrete<P> {
    pub resolve: Option<Addr>,
    pub logical: Logical<P>,
}

#[derive(Clone, Debug)]
pub struct Endpoint<P> {
    pub addr: SocketAddr,
    pub target_addr: SocketAddr,
    pub tls: tls::ConditionalClientTls,
    pub metadata: Metadata,
    pub concrete: Concrete<P>,
}

// === impl Accept ===

impl<P> Param<SocketAddr> for Accept<P> {
    fn param(&self) -> SocketAddr {
        self.orig_dst
    }
}

impl<P> Param<Addr> for Accept<P> {
    fn param(&self) -> Addr {
        self.orig_dst.into()
    }
}

impl<P> Param<transport::labels::Key> for Accept<P> {
    fn param(&self) -> transport::labels::Key {
        const NO_TLS: tls::ConditionalServerTls = Conditional::None(tls::NoServerTls::Loopback);
        transport::labels::Key::accept(transport::labels::Direction::Out, NO_TLS, self.orig_dst)
    }
}

// === impl Logical ===

impl<P> From<(Option<profiles::Receiver>, Accept<P>)> for Logical<P> {
    fn from(
        (
            profile,
            Accept {
                orig_dst, protocol, ..
            },
        ): (Option<profiles::Receiver>, Accept<P>),
    ) -> Self {
        Self {
            profile,
            orig_dst,
            protocol,
        }
    }
}

/// Used for traffic split
impl<P> Param<Option<profiles::Receiver>> for Logical<P> {
    fn param(&self) -> Option<profiles::Receiver> {
        self.profile.clone()
    }
}

/// Used to determine whether detection should be skipped.
impl<P> Param<SocketAddr> for Logical<P> {
    fn param(&self) -> SocketAddr {
        self.orig_dst
    }
}

/// Used for default traffic split
impl<P> Param<profiles::LogicalAddr> for Logical<P> {
    fn param(&self) -> profiles::LogicalAddr {
        profiles::LogicalAddr(self.addr())
    }
}

impl<P> Logical<P> {
    pub fn addr(&self) -> Addr {
        self.profile
            .as_ref()
            .and_then(|p| p.borrow().name.clone())
            .map(|n| Addr::from((n, self.orig_dst.port())))
            .unwrap_or_else(|| self.orig_dst.into())
    }

    pub fn should_resolve(&self) -> bool {
        if let Some(p) = self.profile.as_ref() {
            let p = p.borrow();
            p.endpoint.is_none() && (p.name.is_some() || !p.targets.is_empty())
        } else {
            false
        }
    }
}

impl<P: PartialEq> PartialEq<Logical<P>> for Logical<P> {
    fn eq(&self, other: &Logical<P>) -> bool {
        self.orig_dst == other.orig_dst && self.protocol == other.protocol
    }
}

impl<P: Eq> Eq for Logical<P> {}

impl<P: std::hash::Hash> std::hash::Hash for Logical<P> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.orig_dst.hash(state);
        self.protocol.hash(state);
    }
}

impl<P: std::fmt::Debug> std::fmt::Debug for Logical<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Logical")
            .field("orig_dst", &self.orig_dst)
            .field("protocol", &self.protocol)
            .field(
                "profile",
                &format_args!(
                    "{}",
                    if self.profile.is_some() {
                        "Some(..)"
                    } else {
                        "None"
                    }
                ),
            )
            .finish()
    }
}

// === impl Concrete ===

impl<P> From<(Option<Addr>, Logical<P>)> for Concrete<P> {
    fn from((resolve, logical): (Option<Addr>, Logical<P>)) -> Self {
        Self { resolve, logical }
    }
}

/// Produces an address to be used if resolution is rejected.
impl<P> Param<SocketAddr> for Concrete<P> {
    fn param(&self) -> SocketAddr {
        self.resolve
            .as_ref()
            .and_then(|a| a.socket_addr())
            .unwrap_or(self.logical.orig_dst)
    }
}

// === impl Endpoint ===

impl<P> Endpoint<P> {
    pub fn from_logical(reason: tls::NoClientTls) -> impl (Fn(Logical<P>) -> Self) + Clone {
        move |logical| {
            let target_addr = logical.orig_dst;
            match logical
                .profile
                .as_ref()
                .and_then(|p| p.borrow().endpoint.clone())
            {
                None => Self {
                    addr: logical.param(),
                    metadata: Metadata::default(),
                    tls: Conditional::None(reason),
                    concrete: Concrete {
                        logical,
                        resolve: None,
                    },
                    target_addr,
                },
                Some((addr, metadata)) => Self {
                    addr,
                    tls: EndpointFromMetadata::client_tls(&metadata),
                    metadata,
                    concrete: Concrete {
                        logical,
                        resolve: None,
                    },
                    target_addr,
                },
            }
        }
    }

    pub fn from_accept(reason: tls::NoClientTls) -> impl (Fn(Accept<P>) -> Self) + Clone {
        move |accept| Self::from_logical(reason)(Logical::from((None, accept)))
    }

    /// Marks identity as disabled.
    pub fn identity_disabled(mut self) -> Self {
        self.tls = Conditional::None(tls::NoClientTls::Disabled);
        self
    }
}

impl<P> Param<transport::ConnectAddr> for Endpoint<P> {
    fn param(&self) -> transport::ConnectAddr {
        transport::ConnectAddr(self.addr)
    }
}

impl<P> Param<tls::ConditionalClientTls> for Endpoint<P> {
    fn param(&self) -> tls::ConditionalClientTls {
        self.tls.clone()
    }
}

impl<P> Param<transport::labels::Key> for Endpoint<P> {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::OutboundConnect(self.param())
    }
}

impl<P> Param<metrics::OutboundEndpointLabels> for Endpoint<P> {
    fn param(&self) -> metrics::OutboundEndpointLabels {
        metrics::OutboundEndpointLabels {
            authority: Some(self.concrete.logical.addr().to_http_authority()),
            labels: metrics::prefix_labels("dst", self.metadata.labels().iter()),
            server_id: self.tls.clone(),
            target_addr: self.target_addr,
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
        self.concrete.resolve.hash(state);
        self.concrete.logical.orig_dst.hash(state);
        self.concrete.logical.protocol.hash(state);
    }
}

impl EndpointFromMetadata {
    fn client_tls(metadata: &Metadata) -> tls::ConditionalClientTls {
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
            .unwrap_or(Conditional::None(
                tls::NoClientTls::NotProvidedByServiceDiscovery,
            ))
    }
}

impl<P: Clone + std::fmt::Debug> MapEndpoint<Concrete<P>, Metadata> for EndpointFromMetadata {
    type Out = Endpoint<P>;

    fn map_endpoint(
        &self,
        concrete: &Concrete<P>,
        addr: SocketAddr,
        metadata: Metadata,
    ) -> Self::Out {
        tracing::trace!(%addr, ?metadata, ?concrete, "Resolved endpoint");
        Endpoint {
            addr,
            tls: Self::client_tls(&metadata),
            metadata,
            concrete: concrete.clone(),
            target_addr: concrete.logical.orig_dst,
        }
    }
}
