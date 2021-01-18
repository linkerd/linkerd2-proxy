use linkerd_app_core::{
    metrics, profiles,
    proxy::{api_resolve::Metadata, resolve::map_endpoint::MapEndpoint},
    tls,
    transport::{self, listen},
    Addr, Conditional,
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
    pub identity: tls::Conditional<tls::client::ServerId>,
    pub metadata: Metadata,
    pub concrete: Concrete<P>,
}

// === impl Accept ===

impl From<listen::Addrs> for Accept<()> {
    fn from(addrs: listen::Addrs) -> Self {
        Self {
            orig_dst: addrs.target_addr(),
            protocol: (),
        }
    }
}

impl<P> From<(P, Accept<()>)> for Accept<P> {
    fn from((protocol, Accept { orig_dst, .. }): (P, Accept<()>)) -> Self {
        Self { orig_dst, protocol }
    }
}

impl<P> Into<SocketAddr> for &'_ Accept<P> {
    fn into(self) -> SocketAddr {
        self.orig_dst
    }
}

impl<P> Into<Addr> for &'_ Accept<P> {
    fn into(self) -> Addr {
        self.orig_dst.into()
    }
}

impl<P> Into<transport::labels::Key> for &'_ Accept<P> {
    fn into(self) -> transport::labels::Key {
        const NO_TLS: tls::Conditional<tls::ClientId> =
            Conditional::None(tls::ReasonForNoPeerName::Loopback);
        transport::labels::Key::accept(transport::labels::Direction::Out, NO_TLS)
    }
}

// === impl Logical ===

impl<P> From<(Option<profiles::Receiver>, Accept<P>)> for Logical<P> {
    fn from(
        (profile, Accept { orig_dst, protocol }): (Option<profiles::Receiver>, Accept<P>),
    ) -> Self {
        Self {
            profile,
            orig_dst,
            protocol,
        }
    }
}

/// Used for traffic split
impl<P> Into<Option<profiles::Receiver>> for &'_ Logical<P> {
    fn into(self) -> Option<profiles::Receiver> {
        self.profile.clone()
    }
}

/// Used to determine whether detection should be skipped.
impl<P> Into<SocketAddr> for &'_ Logical<P> {
    fn into(self) -> SocketAddr {
        self.orig_dst
    }
}

/// Used for default traffic split
impl<P> Into<Addr> for &'_ Logical<P> {
    fn into(self) -> Addr {
        self.addr()
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
impl<P> Into<SocketAddr> for &'_ Concrete<P> {
    fn into(self) -> SocketAddr {
        self.resolve
            .as_ref()
            .and_then(|a| a.socket_addr())
            .unwrap_or(self.logical.orig_dst)
    }
}

// === impl Endpoint ===

impl<P> Endpoint<P> {
    pub fn from_logical(reason: tls::ReasonForNoPeerName) -> impl (Fn(Logical<P>) -> Self) + Clone {
        move |logical| match logical
            .profile
            .as_ref()
            .and_then(|p| p.borrow().endpoint.clone())
        {
            None => Self {
                addr: (&logical).into(),
                metadata: Metadata::default(),
                identity: tls::Conditional::None(reason),
                concrete: Concrete {
                    logical,
                    resolve: None,
                },
            },
            Some((addr, metadata)) => Self {
                addr,
                identity: metadata
                    .identity()
                    .cloned()
                    .map(tls::Conditional::Some)
                    .unwrap_or(tls::Conditional::None(
                        tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery,
                    )),
                metadata,
                concrete: Concrete {
                    logical,
                    resolve: None,
                },
            },
        }
    }

    pub fn from_accept(reason: tls::ReasonForNoPeerName) -> impl (Fn(Accept<P>) -> Self) + Clone {
        move |accept| Self::from_logical(reason)(Logical::from((None, accept)))
    }
}

impl<P> Into<SocketAddr> for Endpoint<P> {
    fn into(self) -> SocketAddr {
        self.addr
    }
}

impl<P> Into<SocketAddr> for &'_ Endpoint<P> {
    fn into(self) -> SocketAddr {
        self.addr
    }
}

impl<P> Into<tls::Conditional<tls::client::ServerId>> for &'_ Endpoint<P> {
    fn into(self) -> tls::Conditional<tls::client::ServerId> {
        self.identity.clone()
    }
}

impl<P> Into<transport::labels::Key> for &'_ Endpoint<P> {
    fn into(self) -> transport::labels::Key {
        transport::labels::Key::Connect(self.into())
    }
}

impl<P> Into<metrics::EndpointLabels> for &'_ Endpoint<P> {
    fn into(self) -> metrics::EndpointLabels {
        metrics::EndpointLabels {
            authority: Some(self.concrete.logical.addr().to_http_authority()),
            direction: metrics::Direction::Out,
            labels: metrics::prefix_labels("dst", self.metadata.labels().iter()),
            tls_id: self.identity.clone().into(),
        }
    }
}

impl<P: std::hash::Hash> std::hash::Hash for Endpoint<P> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.addr.hash(state);
        self.identity.hash(state);
        self.concrete.resolve.hash(state);
        self.concrete.logical.orig_dst.hash(state);
        self.concrete.logical.protocol.hash(state);
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
        let identity = metadata
            .identity()
            .cloned()
            .map(Conditional::Some)
            .unwrap_or_else(|| {
                Conditional::None(tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery)
            });

        Endpoint {
            addr,
            identity,
            metadata,
            concrete: concrete.clone(),
        }
    }
}
