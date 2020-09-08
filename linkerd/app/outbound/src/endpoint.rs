use crate::http::uri::Authority;
use indexmap::IndexMap;
use linkerd2_app_core::{
    dst, metric_labels,
    metric_labels::{prefix_labels, EndpointLabels},
    profiles,
    proxy::{
        api_resolve::{Metadata, ProtocolHint},
        http::override_authority::CanOverrideAuthority,
        http::{self, identity_from_header, Settings},
        identity,
        resolve::map_endpoint::MapEndpoint,
        tap,
    },
    router,
    transport::{listen, tls},
    Addr, Conditional, L5D_REQUIRE_ID,
};
use std::net::SocketAddr;
use std::sync::Arc;

#[derive(Copy, Clone, Debug)]
pub struct FromMetadata;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct HttpLogical {
    pub dst: Addr,
    pub orig_dst: SocketAddr,
    pub settings: http::Settings,
    pub require_identity: Option<identity::Name>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpConcrete {
    pub dst: Addr,
    pub logical: HttpLogical,
}

#[derive(Clone, Debug)]
pub struct LogicalPerRequest(listen::Addrs);

#[derive(Clone, Debug)]
pub struct Profile {
    pub rx: profiles::Receiver,
    pub logical: HttpLogical,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpEndpoint {
    pub addr: SocketAddr,
    pub settings: http::Settings,
    pub identity: tls::PeerIdentity,
    pub metadata: Metadata,
    pub concrete: HttpConcrete,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TcpEndpoint {
    pub addr: SocketAddr,
    pub identity: tls::PeerIdentity,
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
            settings: logical.settings,
            identity: logical
                .require_identity
                .clone()
                .map(Conditional::Some)
                .unwrap_or_else(|| {
                    Conditional::None(
                        tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery.into(),
                    )
                }),
            concrete: logical.into(),
            metadata: Metadata::empty(),
        }
    }
}

impl HttpEndpoint {
    pub fn can_use_orig_proto(&self) -> bool {
        if let ProtocolHint::Unknown = self.metadata.protocol_hint() {
            return false;
        }

        // Look at the original settings, ignoring any authority overrides.
        match self.settings {
            http::Settings::Http2 => false,
            http::Settings::Http1 {
                wants_h1_upgrade, ..
            } => !wants_h1_upgrade,
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

impl AsRef<http::Settings> for HttpEndpoint {
    fn as_ref(&self) -> &http::Settings {
        &self.settings
    }
}

impl http::normalize_uri::ShouldNormalizeUri for HttpEndpoint {
    fn should_normalize_uri(&self) -> Option<http::uri::Authority> {
        if let http::Settings::Http1 {
            was_absolute_form: false,
            ..
        } = self.settings
        {
            return Some(self.concrete.logical.dst.to_http_authority());
        }
        None
    }
}

impl tap::Inspect for HttpEndpoint {
    fn src_addr<B>(&self, req: &http::Request<B>) -> Option<SocketAddr> {
        req.extensions().get::<listen::Addrs>().map(|s| s.peer())
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
        let identity = concrete
            .logical
            .require_identity
            .as_ref()
            .or_else(|| metadata.identity())
            .cloned()
            .map(Conditional::Some)
            .unwrap_or_else(|| {
                Conditional::None(tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery.into())
            });

        let settings = match concrete.logical.settings {
            Settings::Http1 {
                keep_alive,
                wants_h1_upgrade,
                was_absolute_form,
            } => Settings::Http1 {
                keep_alive,
                wants_h1_upgrade,
                // Always use absolute form when an onverride is present.
                was_absolute_form: metadata.authority_override().is_some() || was_absolute_form,
            },
            settings => settings,
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
        use linkerd2_app_core::metric_labels::{Direction, TlsId};
        EndpointLabels {
            authority: Some(self.concrete.logical.dst.to_http_authority()),
            direction: Direction::Out,
            tls_id: self.identity.as_ref().map(|id| TlsId::ServerId(id.clone())),
            labels: prefix_labels("dst", self.metadata.labels().into_iter()),
        }
    }
}

// === impl TcpEndpoint ===

impl From<listen::Addrs> for TcpEndpoint {
    fn from(addrs: listen::Addrs) -> Self {
        Self {
            addr: addrs.target_addr(),
            identity: Conditional::None(tls::ReasonForNoPeerName::NotHttp.into()),
        }
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
        use linkerd2_app_core::metric_labels::{Direction, TlsId};
        EndpointLabels {
            direction: Direction::Out,
            tls_id: self.identity.as_ref().map(|id| TlsId::ServerId(id.clone())),
            authority: None,
            labels: None,
        }
    }
}

// === impl LogicalPerRequest ===

impl From<listen::Addrs> for LogicalPerRequest {
    fn from(t: listen::Addrs) -> Self {
        LogicalPerRequest(t)
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
                let addr = self.0.target_addr();
                tracing::debug!(%addr, "using socket target");
                addr.into()
            });

        let settings = http::Settings::from_request(req);

        tracing::debug!(headers = ?req.headers(), uri = %req.uri(), dst = %dst, http.settings = ?settings, "Setting target for request");

        let require_identity = identity_from_header(req, L5D_REQUIRE_ID);

        HttpLogical {
            dst,
            orig_dst: self.0.target_addr(),
            settings,
            require_identity,
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
