use linkerd_app_core::{
    metrics, profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        resolve::map_endpoint::MapEndpoint,
    },
    svc::{self, Param},
    tls,
    transport::{self, OrigDstAddr, Remote, ServerAddr},
    transport_header, Addr, Conditional, Error,
};
use std::net::SocketAddr;
use tracing::debug;

#[derive(Copy, Clone)]
pub struct EndpointFromMetadata {
    pub identity_disabled: bool,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Accept<P> {
    pub orig_dst: OrigDstAddr,
    pub protocol: P,
}

#[derive(Clone)]
pub struct Logical<P> {
    pub orig_dst: OrigDstAddr,
    pub profile: Option<profiles::Receiver>,
    pub protocol: P,
}

#[derive(Clone, Debug)]
pub struct Concrete<P> {
    pub resolve: ConcreteAddr,
    pub logical: Logical<P>,
}

#[derive(Clone, Debug)]
pub struct Endpoint<P> {
    pub addr: Remote<ServerAddr>,
    pub tls: tls::ConditionalClientTls,
    pub metadata: Metadata,
    pub logical_addr: Addr,
    pub protocol: P,
}

// === impl Accept ===

impl<P> Param<transport::labels::Key> for Accept<P> {
    fn param(&self) -> transport::labels::Key {
        const NO_TLS: tls::ConditionalServerTls = Conditional::None(tls::NoServerTls::Loopback);
        transport::labels::Key::accept(
            transport::labels::Direction::Out,
            NO_TLS,
            self.orig_dst.into(),
        )
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
            .map(|n| Addr::from((n, self.orig_dst.0.port())))
            .unwrap_or_else(|| self.orig_dst.0.into())
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

impl<P> Logical<P> {
    pub fn or_endpoint(
        reason: tls::NoClientTls,
    ) -> impl Fn(Self) -> Result<svc::Either<Self, Endpoint<P>>, Error> + Copy {
        move |logical: Self| {
            let should_resolve = match logical.profile.as_ref() {
                Some(p) => {
                    let p = p.borrow();
                    p.endpoint.is_none() && (p.name.is_some() || !p.targets.is_empty())
                }
                None => false,
            };

            if should_resolve {
                Ok(svc::Either::A(logical))
            } else {
                debug!(%reason, orig_dst = %logical.orig_dst, "Target is unresolveable");
                Ok(svc::Either::B(Endpoint::from((reason, logical))))
            }
        }
    }
}

// === impl Concrete ===

impl<P> From<(ConcreteAddr, Logical<P>)> for Concrete<P> {
    fn from((resolve, logical): (ConcreteAddr, Logical<P>)) -> Self {
        Self { resolve, logical }
    }
}

impl<P> Param<ConcreteAddr> for Concrete<P> {
    fn param(&self) -> ConcreteAddr {
        self.resolve.clone()
    }
}

// === impl Endpoint ===

impl<P> From<(tls::NoClientTls, Logical<P>)> for Endpoint<P> {
    fn from((reason, logical): (tls::NoClientTls, Logical<P>)) -> Self {
        match logical
            .profile
            .as_ref()
            .and_then(|p| p.borrow().endpoint.clone())
        {
            None => Self {
                addr: Remote(ServerAddr(logical.orig_dst.into())),
                metadata: Metadata::default(),
                tls: Conditional::None(reason),
                logical_addr: logical.addr(),
                protocol: logical.protocol,
            },
            Some((addr, metadata)) => Self {
                addr: Remote(ServerAddr(addr)),
                tls: EndpointFromMetadata::client_tls(&metadata),
                metadata,
                logical_addr: logical.addr(),
                protocol: logical.protocol,
            },
        }
    }
}

impl<P> From<(tls::NoClientTls, Accept<P>)> for Endpoint<P> {
    fn from((reason, accept): (tls::NoClientTls, Accept<P>)) -> Self {
        Self::from((reason, Logical::from((None, accept))))
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

impl<P> Param<transport::labels::Key> for Endpoint<P> {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::OutboundConnect(self.param())
    }
}

impl<P> Param<metrics::OutboundEndpointLabels> for Endpoint<P> {
    fn param(&self) -> metrics::OutboundEndpointLabels {
        metrics::OutboundEndpointLabels {
            authority: Some(self.logical_addr.to_http_authority()),
            labels: metrics::prefix_labels("dst", self.metadata.labels().iter()),
            server_id: self.tls.clone(),
            target_addr: self.addr.into(),
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

impl<P: Copy + std::fmt::Debug> MapEndpoint<Concrete<P>, Metadata> for EndpointFromMetadata {
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
            Self::client_tls(&metadata)
        };
        Endpoint {
            addr: Remote(ServerAddr(addr)),
            tls,
            metadata,
            logical_addr: concrete.logical.addr(),
            protocol: concrete.logical.protocol,
        }
    }
}
