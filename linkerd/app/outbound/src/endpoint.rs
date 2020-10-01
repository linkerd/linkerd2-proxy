use crate::http::uri::Authority;
use indexmap::IndexMap;
use linkerd2_app_core::{
    dst,
    metric_labels,
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
    router,
    transport::{listen, tls},
    Addr,
    Conditional, //L5D_REQUIRE_ID,
};
use std::{net::SocketAddr, sync::Arc};

#[derive(Copy, Clone, Debug)]
pub struct FromMetadata;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct HttpAccept {
    pub orig_dst: SocketAddr,
    pub version: http::Version,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct HttpLogical {
    pub dst: Addr,
    pub orig_dst: SocketAddr,
    pub version: http::Version,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpConcrete {
    pub dst: Addr,
    pub logical: HttpLogical,
}

#[derive(Clone, Debug)]
pub struct LogicalPerRequest(HttpAccept);

#[derive(Clone, Debug)]
pub struct Profile {
    pub rx: profiles::Receiver,
    pub logical: HttpLogical,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpEndpoint {
    pub addr: SocketAddr,
    pub settings: http::client::Settings,
    pub identity: tls::PeerIdentity,
    pub metadata: Metadata,
    pub concrete: HttpConcrete,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct TcpLogical {
    pub addr: SocketAddr,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TcpEndpoint {
    pub dst: Addr,
    pub addr: SocketAddr,
    pub identity: tls::PeerIdentity,
    pub labels: Option<String>,
}

impl From<listen::Addrs> for TcpLogical {
    fn from(addrs: listen::Addrs) -> Self {
        Self {
            addr: addrs.target_addr(),
        }
    }
}

impl Into<SocketAddr> for &'_ TcpLogical {
    fn into(self) -> SocketAddr {
        self.addr
    }
}

impl Into<Addr> for &'_ TcpLogical {
    fn into(self) -> Addr {
        self.addr.into()
    }
}

impl std::fmt::Display for TcpLogical {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.addr.fmt(f)
    }
}

// === impl HttpAccept ===

impl From<(http::Version, TcpLogical)> for HttpAccept {
    fn from((version, TcpLogical { addr }): (http::Version, TcpLogical)) -> Self {
        Self {
            version,
            orig_dst: addr,
        }
    }
}

impl Into<SocketAddr> for &'_ HttpAccept {
    fn into(self) -> SocketAddr {
        self.orig_dst
    }
}

// === impl HttpConrete ===

impl From<(Addr, Profile)> for HttpConcrete {
    fn from((dst, Profile { logical, .. }): (Addr, Profile)) -> Self {
        Self { dst, logical }
    }
}

impl AsRef<Addr> for HttpConcrete {
    fn as_ref(&self) -> &Addr {
        &self.dst
    }
}

impl Into<Addr> for &'_ HttpConcrete {
    fn into(self) -> Addr {
        self.dst.clone()
    }
}

impl std::fmt::Display for HttpConcrete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.dst.fmt(f)
    }
}

impl From<HttpLogical> for HttpConcrete {
    fn from(logical: HttpLogical) -> Self {
        Self {
            dst: logical.dst.clone(),
            logical,
        }
    }
}

// === impl HttpLogical ===

impl std::fmt::Display for HttpLogical {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.dst.fmt(f)
    }
}

impl<'t> From<&'t HttpLogical> for http::header::HeaderValue {
    fn from(target: &'t HttpLogical) -> Self {
        http::header::HeaderValue::from_str(&target.dst.to_string())
            .expect("addr must be a valid header")
    }
}

impl Into<SocketAddr> for HttpLogical {
    fn into(self) -> SocketAddr {
        self.orig_dst
    }
}

impl Into<Addr> for &'_ HttpLogical {
    fn into(self) -> Addr {
        self.dst.clone()
    }
}

impl AsRef<Addr> for HttpLogical {
    fn as_ref(&self) -> &Addr {
        &self.dst
    }
}

impl AsMut<Addr> for HttpLogical {
    fn as_mut(&mut self) -> &mut Addr {
        &mut self.dst
    }
}

// === impl HttpEndpoint ===

impl From<HttpLogical> for HttpEndpoint {
    fn from(logical: HttpLogical) -> Self {
        Self {
            addr: logical.orig_dst,
            settings: logical.version.into(),
            identity: Conditional::None(
                tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery.into(),
            ),
            concrete: logical.into(),
            metadata: Metadata::empty(),
        }
    }
}

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

impl Into<EndpointLabels> for HttpEndpoint {
    fn into(self) -> EndpointLabels {
        use linkerd2_app_core::metric_labels::Direction;
        EndpointLabels {
            authority: Some(self.concrete.logical.dst.to_http_authority()),
            direction: Direction::Out,
            tls_id: TlsStatus::server(self.identity.clone()),
            labels: prefix_labels("dst", self.metadata.labels().into_iter()),
        }
    }
}

// === impl TcpEndpoint ===

impl From<SocketAddr> for TcpEndpoint {
    fn from(addr: SocketAddr) -> Self {
        Self {
            addr,
            dst: addr.into(),
            identity: Conditional::None(tls::ReasonForNoPeerName::NotHttp.into()),
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
        self.addr
    }
}

impl tls::HasPeerIdentity for TcpEndpoint {
    fn peer_identity(&self) -> tls::PeerIdentity {
        self.identity.clone()
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

impl MapEndpoint<TcpLogical, Metadata> for FromMetadata {
    type Out = TcpEndpoint;

    fn map_endpoint(
        &self,
        logical: &TcpLogical,
        addr: SocketAddr,
        metadata: Metadata,
    ) -> Self::Out {
        tracing::debug!(?logical, %addr, ?metadata, "Resolved endpoint");
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
            dst: logical.addr.into(),
            labels: prefix_labels("dst", metadata.labels().into_iter()),
        }
    }
}

// === impl LogicalPerRequest ===

impl From<HttpAccept> for LogicalPerRequest {
    fn from(accept: HttpAccept) -> Self {
        LogicalPerRequest(accept)
    }
}

impl<B> router::Recognize<http::Request<B>> for LogicalPerRequest {
    type Key = HttpLogical;

    fn recognize(&self, req: &http::Request<B>) -> Self::Key {
        use linkerd2_app_core::{
            http_request_authority_addr, http_request_host_addr, http_request_l5d_override_dst_addr,
        };

        let dst = http_request_l5d_override_dst_addr(req)
            .map(|addr| {
                tracing::debug!(%addr, "using dst-override");
                addr
            })
            .or_else(|_| {
                http_request_authority_addr(req).map(|addr| {
                    tracing::debug!(%addr, "using authority");
                    addr
                })
            })
            .or_else(|_| {
                http_request_host_addr(req).map(|addr| {
                    tracing::debug!(%addr, "using host");
                    addr
                })
            })
            .unwrap_or_else(|_| {
                let addr = self.0.orig_dst;
                tracing::debug!(%addr, "using socket target");
                addr.into()
            });

        tracing::debug!(headers = ?req.headers(), uri = %req.uri(), dst = %dst, version = ?req.version(), "Setting target for request");

        HttpLogical {
            dst,
            orig_dst: self.0.orig_dst,
            version: self.0.version,
        }
    }
}

pub fn route((route, profile): (profiles::http::Route, Profile)) -> dst::Route {
    dst::Route {
        route,
        target: profile.logical.dst,
        direction: metric_labels::Direction::Out,
    }
}

// === impl Profile ===

impl From<(profiles::Receiver, HttpLogical)> for Profile {
    fn from((rx, logical): (profiles::Receiver, HttpLogical)) -> Self {
        Self { rx, logical }
    }
}

impl AsRef<Addr> for Profile {
    fn as_ref(&self) -> &Addr {
        &self.logical.dst
    }
}

impl AsRef<profiles::Receiver> for Profile {
    fn as_ref(&self) -> &profiles::Receiver {
        &self.rx
    }
}

impl From<Profile> for HttpLogical {
    fn from(Profile { logical, .. }: Profile) -> Self {
        logical
    }
}
