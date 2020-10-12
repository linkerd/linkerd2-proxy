use crate::http::uri::Authority;
use indexmap::IndexMap;
use linkerd2_app_core::{
    dst, metric_labels,
    metric_labels::{prefix_labels, EndpointLabels, TlsStatus},
    profiles,
    proxy::{
        api_resolve::{Metadata, ProtocolHint},
        http::override_authority::CanOverrideAuthority,
        http::{self},
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

#[derive(Clone)]
pub struct HttpLogical {
    pub orig_dst: SocketAddr,
    pub version: http::Version,
    pub profile: Option<profiles::Receiver>,
}

#[derive(Clone, Debug)]
pub struct HttpConcrete {
    pub resolve: Option<Addr>,
    pub logical: HttpLogical,
}

#[derive(Clone, Debug)]
pub struct HttpEndpoint {
    pub addr: SocketAddr,
    pub settings: http::client::Settings,
    pub identity: tls::PeerIdentity,
    pub metadata: Metadata,
    pub concrete: HttpConcrete,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct TcpAccept {
    pub addr: SocketAddr,
}

#[derive(Clone)]
pub struct TcpLogical {
    pub addr: SocketAddr,
    pub profile: Option<profiles::Receiver>,
}

#[derive(Clone, Debug)]
pub struct TcpConcrete {
    pub resolve: Option<Addr>,
    pub logical: TcpLogical,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TcpEndpoint {
    pub dst: Addr,
    pub addr: SocketAddr,
    pub identity: tls::PeerIdentity,
    pub labels: Option<String>,
}

// === impl TcpAccept ===

impl From<listen::Addrs> for TcpAccept {
    fn from(addrs: listen::Addrs) -> Self {
        Self {
            addr: addrs.target_addr(),
        }
    }
}

impl Into<Addr> for &'_ TcpAccept {
    fn into(self) -> Addr {
        self.addr.into()
    }
}

impl Into<transport::labels::Key> for &'_ TcpAccept {
    fn into(self) -> transport::labels::Key {
        const NO_TLS: tls::Conditional<identity::Name> =
            Conditional::None(tls::ReasonForNoPeerName::Loopback);
        transport::labels::Key::accept(transport::labels::Direction::Out, NO_TLS.into())
    }
}

// === impl TcpLogical ===

impl From<(Option<profiles::Receiver>, TcpAccept)> for TcpLogical {
    fn from((profile, TcpAccept { addr }): (Option<profiles::Receiver>, TcpAccept)) -> Self {
        Self { addr, profile }
    }
}

/// Used for default traffic split
impl Into<Addr> for &'_ TcpLogical {
    fn into(self) -> Addr {
        self.addr.into()
    }
}

/// Used for traffic split
impl Into<Option<profiles::Receiver>> for &'_ TcpLogical {
    fn into(self) -> Option<profiles::Receiver> {
        self.profile.clone()
    }
}

/// Used to determine whether detection should be skipped.
impl Into<SocketAddr> for &'_ TcpLogical {
    fn into(self) -> SocketAddr {
        self.addr
    }
}

impl std::fmt::Debug for TcpLogical {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TcpLogical")
            .field("addr", &self.addr)
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

// === impl TcpConcrete ===

impl From<(Option<Addr>, TcpLogical)> for TcpConcrete {
    fn from((resolve, logical): (Option<Addr>, TcpLogical)) -> Self {
        Self { resolve, logical }
    }
}

/// Used as a default destination when resolution is rejected.
impl Into<SocketAddr> for &'_ TcpConcrete {
    fn into(self) -> SocketAddr {
        self.logical.addr
    }
}

// === impl HttpLogical ===

impl HttpLogical {
    pub fn addr(&self) -> Addr {
        self.profile
            .as_ref()
            .and_then(|p| p.borrow().name.clone())
            .map(|n| Addr::from((n, self.orig_dst.port())))
            .unwrap_or_else(|| self.orig_dst.into())
    }
}

impl From<(http::Version, TcpLogical)> for HttpLogical {
    fn from((version, TcpLogical { addr, profile }): (http::Version, TcpLogical)) -> Self {
        Self {
            version,
            orig_dst: addr,
            profile,
        }
    }
}

/// For normalization when no host is present.
impl Into<SocketAddr> for &'_ HttpLogical {
    fn into(self) -> SocketAddr {
        self.orig_dst
    }
}

impl Into<Addr> for &'_ HttpLogical {
    fn into(self) -> Addr {
        self.addr()
    }
}

impl Into<Option<profiles::Receiver>> for &'_ HttpLogical {
    fn into(self) -> Option<profiles::Receiver> {
        self.profile.clone()
    }
}

// Used to set the l5d-canonical-dst header.
impl<'t> From<&'t HttpLogical> for http::header::HeaderValue {
    fn from(target: &'t HttpLogical) -> Self {
        http::header::HeaderValue::from_str(&target.addr().to_string())
            .expect("addr must be a valid header")
    }
}

impl std::fmt::Debug for HttpLogical {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpLogical")
            .field("version", &self.version)
            .field("orig_dst", &self.orig_dst)
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

// === impl HttpConrete ===

impl From<(Option<Addr>, HttpLogical)> for HttpConcrete {
    fn from((resolve, logical): (Option<Addr>, HttpLogical)) -> Self {
        Self { resolve, logical }
    }
}

/// Produces an address to resolve to individual endpoints. This address is only
/// present if the initial profile resolution was not rejected.
impl Into<Option<Addr>> for &'_ HttpConcrete {
    fn into(self) -> Option<Addr> {
        self.resolve.clone()
    }
}

/// Produces an address to be used if resolution is rejected.
impl Into<SocketAddr> for &'_ HttpConcrete {
    fn into(self) -> SocketAddr {
        self.resolve
            .as_ref()
            .and_then(|a| a.socket_addr())
            .unwrap_or_else(|| self.logical.orig_dst)
    }
}

// === impl HttpEndpoint ===

impl std::hash::Hash for HttpEndpoint {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.addr.hash(state);
        self.identity.hash(state);
        self.settings.hash(state);
    }
}

impl tls::HasPeerIdentity for HttpEndpoint {
    fn peer_identity(&self) -> tls::PeerIdentity {
        self.identity.clone()
    }
}

impl Into<SocketAddr> for HttpEndpoint {
    fn into(self) -> SocketAddr {
        (&self).into()
    }
}

impl Into<SocketAddr> for &'_ HttpEndpoint {
    fn into(self) -> SocketAddr {
        self.addr
    }
}

impl Into<http::client::Settings> for &'_ HttpEndpoint {
    fn into(self) -> http::client::Settings {
        self.settings
    }
}

impl tap::Inspect for HttpEndpoint {
    fn src_addr<B>(&self, _: &http::Request<B>) -> Option<SocketAddr> {
        None
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

impl MapEndpoint<HttpConcrete, Metadata> for FromMetadata {
    type Out = HttpEndpoint;

    fn map_endpoint(
        &self,
        concrete: &HttpConcrete,
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

        let settings = match concrete.logical.version {
            http::Version::H2 => http::client::Settings::H2,
            http::Version::Http1 => match metadata.protocol_hint() {
                ProtocolHint::Unknown => http::client::Settings::Http1,
                ProtocolHint::Http2 => http::client::Settings::OrigProtoUpgrade,
            },
        };

        HttpEndpoint {
            addr,
            identity,
            metadata,
            settings,
            concrete: concrete.clone(),
        }
    }
}

impl CanOverrideAuthority for HttpEndpoint {
    fn override_authority(&self) -> Option<Authority> {
        self.metadata.authority_override().cloned()
    }
}

impl Into<transport::labels::Key> for &'_ HttpEndpoint {
    fn into(self) -> transport::labels::Key {
        transport::labels::Key::Connect(self.into())
    }
}

impl Into<EndpointLabels> for &'_ HttpEndpoint {
    fn into(self) -> EndpointLabels {
        use linkerd2_app_core::metric_labels::Direction;
        EndpointLabels {
            direction: Direction::Out,
            authority: Some(self.concrete.logical.addr().to_http_authority()),
            tls_id: TlsStatus::server(self.identity.clone()),
            labels: prefix_labels("dst", self.metadata.labels().iter()),
        }
    }
}

// === impl TcpEndpoint ===

impl From<SocketAddr> for TcpEndpoint {
    fn from(addr: SocketAddr) -> Self {
        Self {
            addr,
            dst: addr.into(),
            identity: Conditional::None(tls::ReasonForNoPeerName::PortSkipped.into()),
            labels: None,
        }
    }
}

impl From<TcpLogical> for TcpEndpoint {
    fn from(l: TcpLogical) -> Self {
        l.addr.into()
    }
}

impl Into<SocketAddr> for TcpEndpoint {
    fn into(self) -> SocketAddr {
        (&self).into()
    }
}

impl Into<SocketAddr> for &'_ TcpEndpoint {
    fn into(self) -> SocketAddr {
        self.addr
    }
}

impl tls::HasPeerIdentity for TcpEndpoint {
    fn peer_identity(&self) -> tls::PeerIdentity {
        self.identity.clone()
    }
}

impl Into<transport::labels::Key> for &'_ TcpEndpoint {
    fn into(self) -> transport::labels::Key {
        transport::labels::Key::Connect(self.clone().into())
    }
}

impl Into<EndpointLabels> for TcpEndpoint {
    fn into(self) -> EndpointLabels {
        use linkerd2_app_core::metric_labels::Direction;
        EndpointLabels {
            authority: Some(self.dst.to_http_authority()),
            direction: Direction::Out,
            labels: self.labels,
            tls_id: TlsStatus::server(self.identity),
        }
    }
}

impl MapEndpoint<TcpConcrete, Metadata> for FromMetadata {
    type Out = TcpEndpoint;

    fn map_endpoint(
        &self,
        concrete: &TcpConcrete,
        addr: SocketAddr,
        metadata: Metadata,
    ) -> Self::Out {
        tracing::debug!(?concrete, %addr, ?metadata, "Resolved endpoint");
        let identity = metadata
            .identity()
            .cloned()
            .map(Conditional::Some)
            .unwrap_or_else(|| {
                Conditional::None(tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery.into())
            });

        TcpEndpoint {
            addr,
            identity,
            dst: concrete.logical.addr.into(),
            labels: prefix_labels("dst", metadata.labels().into_iter()),
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
