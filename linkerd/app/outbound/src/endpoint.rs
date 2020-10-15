use crate::http::uri::Authority;
use indexmap::IndexMap;
use linkerd2_app_core::{
    dst, metric_labels,
    metric_labels::{prefix_labels, EndpointLabels, TlsStatus},
    profiles,
    proxy::{
        api_resolve::{Metadata, ProtocolHint},
        http::{self, override_authority::CanOverrideAuthority, ClientAddr},
        identity,
        resolve::map_endpoint::MapEndpoint,
        tap,
    },
    transport::{self, listen, tls},
    Addr, Conditional,
};
use std::{net::SocketAddr, sync::Arc};

#[derive(Copy, Clone)]
pub struct FromMetadata;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Accept {
    pub orig_dst: SocketAddr,
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
    pub identity: tls::PeerIdentity,
    pub metadata: Metadata,
    pub concrete: Concrete<P>,
}

pub type HttpLogical = Logical<http::Version>;
pub type HttpConcrete = Concrete<http::Version>;
pub type HttpEndpoint = Endpoint<http::Version>;

pub type TcpLogical = Logical<()>;
pub type TcpConcrete = Concrete<()>;
pub type TcpEndpoint = Endpoint<()>;

// === impl Accept ===

impl From<listen::Addrs> for Accept {
    fn from(addrs: listen::Addrs) -> Self {
        Self {
            orig_dst: addrs.target_addr(),
        }
    }
}

impl Into<Addr> for &'_ Accept {
    fn into(self) -> Addr {
        self.orig_dst.into()
    }
}

impl Into<transport::labels::Key> for &'_ Accept {
    fn into(self) -> transport::labels::Key {
        const NO_TLS: tls::Conditional<identity::Name> =
            Conditional::None(tls::ReasonForNoPeerName::Loopback);
        transport::labels::Key::accept(transport::labels::Direction::Out, NO_TLS.into())
    }
}

// === impl Logical ===

impl From<(Option<profiles::Receiver>, Accept)> for TcpLogical {
    fn from((profile, Accept { orig_dst }): (Option<profiles::Receiver>, Accept)) -> Self {
        Self {
            orig_dst,
            profile,
            protocol: (),
        }
    }
}

impl From<(http::Version, TcpLogical)> for HttpLogical {
    fn from((protocol, l): (http::Version, TcpLogical)) -> Self {
        Self {
            orig_dst: l.orig_dst,
            profile: l.profile,
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
}

// Used to set the l5d-canonical-dst header.
impl<P> From<&'_ Logical<P>> for http::header::HeaderValue {
    fn from(target: &'_ Logical<P>) -> Self {
        http::header::HeaderValue::from_str(&target.addr().to_string())
            .expect("addr must be a valid header")
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
            .unwrap_or_else(|| self.logical.orig_dst)
    }
}

// === impl Endpoint ===

impl<P> From<Logical<P>> for Endpoint<P> {
    fn from(logical: Logical<P>) -> Self {
        Self {
            addr: (&logical).into(),
            metadata: Metadata::default(),
            identity: tls::PeerIdentity::None(tls::ReasonForNoPeerName::PortSkipped.into()),
            concrete: Concrete {
                logical,
                resolve: None,
            },
        }
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

impl<P> tls::HasPeerIdentity for Endpoint<P> {
    fn peer_identity(&self) -> tls::PeerIdentity {
        self.identity.clone()
    }
}

impl<P> Into<transport::labels::Key> for &'_ Endpoint<P> {
    fn into(self) -> transport::labels::Key {
        transport::labels::Key::Connect(self.clone().into())
    }
}

impl<P> Into<EndpointLabels> for &'_ Endpoint<P> {
    fn into(self) -> EndpointLabels {
        use linkerd2_app_core::metric_labels::Direction;
        EndpointLabels {
            authority: Some(self.concrete.logical.addr().to_http_authority()),
            direction: Direction::Out,
            labels: prefix_labels("dst", self.metadata.labels().iter()),
            tls_id: TlsStatus::server(self.identity.clone()),
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

impl Into<http::client::Settings> for &'_ HttpEndpoint {
    fn into(self) -> http::client::Settings {
        match self.concrete.logical.protocol {
            http::Version::H2 => http::client::Settings::H2,
            http::Version::Http1 => match self.metadata.protocol_hint() {
                ProtocolHint::Unknown => http::client::Settings::Http1,
                ProtocolHint::Http2 => http::client::Settings::OrigProtoUpgrade,
            },
        }
    }
}

impl<P> CanOverrideAuthority for Endpoint<P> {
    fn override_authority(&self) -> Option<Authority> {
        self.metadata.authority_override().cloned()
    }
}

impl tap::Inspect for HttpEndpoint {
    fn src_addr<B>(&self, req: &http::Request<B>) -> Option<SocketAddr> {
        req.extensions()
            .get::<ClientAddr>()
            .map(|c| c.as_ref().clone())
    }

    fn src_tls<'a, B>(
        &self,
        _: &'a http::Request<B>,
    ) -> Conditional<&'a identity::Name, tls::ReasonForNoPeerName> {
        Conditional::None(tls::ReasonForNoPeerName::Loopback.into())
    }

    fn dst_addr<B>(&self, _: &http::Request<B>) -> Option<SocketAddr> {
        Some(self.addr)
    }

    fn dst_labels<B>(&self, _: &http::Request<B>) -> Option<&IndexMap<String, String>> {
        Some(self.metadata.labels())
    }

    fn dst_tls<B>(
        &self,
        _: &http::Request<B>,
    ) -> Conditional<&identity::Name, tls::ReasonForNoPeerName> {
        self.identity.as_ref()
    }

    fn route_labels<B>(&self, req: &http::Request<B>) -> Option<Arc<IndexMap<String, String>>> {
        req.extensions()
            .get::<dst::Route>()
            .map(|r| r.route.labels().clone())
    }

    fn is_outbound<B>(&self, _: &http::Request<B>) -> bool {
        true
    }
}

impl<P: Clone + std::fmt::Debug> MapEndpoint<Concrete<P>, Metadata> for FromMetadata {
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
                Conditional::None(tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery.into())
            });

        Endpoint {
            addr,
            identity,
            metadata,
            concrete: concrete.clone(),
        }
    }
}

pub fn route((route, logical): (profiles::http::Route, HttpLogical)) -> dst::Route {
    dst::Route {
        route,
        target: logical.addr(),
        direction: metric_labels::Direction::Out,
    }
}
