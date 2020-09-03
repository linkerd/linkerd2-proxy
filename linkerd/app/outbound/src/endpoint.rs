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

pub type Logical<T> = Target<T>;

pub type Concrete<T> = Target<Logical<T>>;

#[derive(Clone, Debug)]
pub struct LogicalPerRequest(listen::Addrs);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Target<T> {
    pub addr: Addr,
    pub inner: T,
}

#[derive(Clone, Debug)]
pub struct Profile {
    pub rx: profiles::Receiver,
    pub target: Target<HttpEndpoint>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpEndpoint {
    pub addr: SocketAddr,
    pub settings: http::Settings,
    pub identity: tls::PeerIdentity,
    pub metadata: Metadata,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TcpEndpoint {
    pub addr: SocketAddr,
    pub identity: tls::PeerIdentity,
}

impl From<(Addr, Profile)> for Concrete<http::Settings> {
    fn from((addr, Profile { target, .. }): (Addr, Profile)) -> Self {
        Self {
            addr,
            inner: target.map(|e| e.settings),
        }
    }
}

// === impl Target ===

impl<T> Target<T> {
    pub fn map<U, F: Fn(T) -> U>(self, map: F) -> Target<U> {
        Target {
            addr: self.addr,
            inner: (map)(self.inner),
        }
    }
}

impl<T> std::fmt::Display for Target<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.addr.fmt(f)
    }
}

impl<T> http::canonicalize::Target for Target<T> {
    fn addr(&self) -> &Addr {
        &self.addr
    }

    fn addr_mut(&mut self) -> &mut Addr {
        &mut self.addr
    }
}

impl<'t, T> From<&'t Target<T>> for http::header::HeaderValue {
    fn from(target: &'t Target<T>) -> Self {
        http::header::HeaderValue::from_str(&target.addr.to_string())
            .expect("addr must be a valid header")
    }
}

impl<T: http::settings::HasSettings> http::normalize_uri::ShouldNormalizeUri for Target<T> {
    fn should_normalize_uri(&self) -> Option<http::uri::Authority> {
        if let http::Settings::Http1 {
            was_absolute_form: false,
            ..
        } = self.inner.http_settings()
        {
            return Some(self.addr.to_http_authority());
        }
        None
    }
}

impl<T: http::settings::HasSettings> http::settings::HasSettings for Target<T> {
    fn http_settings(&self) -> http::Settings {
        self.inner.http_settings()
    }
}

impl<T: tls::HasPeerIdentity> tls::HasPeerIdentity for Target<T> {
    fn peer_identity(&self) -> tls::PeerIdentity {
        self.inner.peer_identity()
    }
}

impl<T: tap::Inspect> tap::Inspect for Target<T> {
    fn src_addr<B>(&self, req: &http::Request<B>) -> Option<SocketAddr> {
        self.inner.src_addr(req)
    }

    fn src_tls<'a, B>(
        &self,
        req: &'a http::Request<B>,
    ) -> Conditional<&'a identity::Name, tls::ReasonForNoPeerName> {
        self.inner.src_tls(req)
    }

    fn dst_addr<B>(&self, req: &http::Request<B>) -> Option<SocketAddr> {
        self.inner.dst_addr(req)
    }

    fn dst_labels<B>(&self, req: &http::Request<B>) -> Option<&IndexMap<String, String>> {
        self.inner.dst_labels(req)
    }

    fn dst_tls<B>(
        &self,
        req: &http::Request<B>,
    ) -> Conditional<&identity::Name, tls::ReasonForNoPeerName> {
        self.inner.dst_tls(req)
    }

    fn route_labels<B>(&self, req: &http::Request<B>) -> Option<Arc<IndexMap<String, String>>> {
        self.inner.route_labels(req)
    }

    fn is_outbound<B>(&self, req: &http::Request<B>) -> bool {
        self.inner.is_outbound(req)
    }
}

impl<T: Into<SocketAddr>> Into<SocketAddr> for Target<T> {
    fn into(self) -> SocketAddr {
        self.inner.into()
    }
}

impl<T> AsRef<Addr> for Target<T> {
    fn as_ref(&self) -> &Addr {
        &self.addr
    }
}

// === impl HttpEndpoint ===

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

impl std::fmt::Display for HttpEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.addr.fmt(f)
    }
}

impl std::hash::Hash for HttpEndpoint {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.addr.hash(state);
        self.identity.hash(state);
        http::settings::HasSettings::http_settings(self).hash(state);
        // Ignore metadata.
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

impl http::settings::HasSettings for HttpEndpoint {
    fn http_settings(&self) -> http::Settings {
        match self.settings {
            Settings::Http1 {
                keep_alive,
                wants_h1_upgrade,
                was_absolute_form,
            } => Settings::Http1 {
                keep_alive,
                wants_h1_upgrade,
                // Always use absolute form when an onverride is present.
                was_absolute_form: self.metadata.authority_override().is_some()
                    || was_absolute_form,
            },
            settings => settings,
        }
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

impl MapEndpoint<Concrete<http::Settings>, Metadata> for FromMetadata {
    type Out = Target<HttpEndpoint>;

    fn map_endpoint(
        &self,
        concrete: &Concrete<http::Settings>,
        addr: SocketAddr,
        metadata: Metadata,
    ) -> Self::Out {
        tracing::trace!(%addr, ?metadata, "Resolved endpoint");
        let identity = metadata
            .identity()
            .cloned()
            .map(Conditional::Some)
            .unwrap_or_else(|| {
                Conditional::None(tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery.into())
            });

        Target {
            // Use the logical addr for the target.
            addr: concrete.inner.addr.clone(),
            inner: HttpEndpoint {
                addr,
                identity,
                metadata,
                settings: concrete.inner.inner,
            },
        }
    }
}

impl CanOverrideAuthority for Target<HttpEndpoint> {
    fn override_authority(&self) -> Option<Authority> {
        self.inner.metadata.authority_override().cloned()
    }
}

impl Into<EndpointLabels> for Target<HttpEndpoint> {
    fn into(self) -> EndpointLabels {
        use linkerd2_app_core::metric_labels::{Direction, TlsId};
        EndpointLabels {
            authority: Some(self.addr.to_http_authority()),
            direction: Direction::Out,
            tls_id: self
                .inner
                .identity
                .as_ref()
                .map(|id| TlsId::ServerId(id.clone())),
            labels: prefix_labels("dst", self.inner.metadata.labels().into_iter()),
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
    type Key = Target<HttpEndpoint>;

    fn recognize(&self, req: &http::Request<B>) -> Self::Key {
        use linkerd2_app_core::{
            http_request_authority_addr, http_request_host_addr, http_request_l5d_override_dst_addr,
        };

        let addr = http_request_l5d_override_dst_addr(req)
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

        tracing::debug!(headers = ?req.headers(), uri = %req.uri(), target.addr = %addr, http.settings = ?settings, "Setting target for request");

        let inner = HttpEndpoint {
            settings,
            addr: self.0.target_addr(),
            metadata: Metadata::empty(),
            identity: identity_from_header(req, L5D_REQUIRE_ID)
                .map(Conditional::Some)
                .unwrap_or_else(|| {
                    Conditional::None(
                        tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery.into(),
                    )
                }),
        };

        Target { addr, inner }
    }
}

pub fn route((route, profile): (profiles::http::Route, Profile)) -> dst::Route {
    dst::Route {
        route,
        target: profile.target.addr,
        direction: metric_labels::Direction::Out,
    }
}

// === impl Profile ===

impl From<(profiles::Receiver, Target<HttpEndpoint>)> for Profile {
    fn from((rx, target): (profiles::Receiver, Target<HttpEndpoint>)) -> Self {
        Self { rx, target }
    }
}

impl AsRef<Addr> for Profile {
    fn as_ref(&self) -> &Addr {
        &self.target.addr
    }
}

impl AsRef<profiles::Receiver> for Profile {
    fn as_ref(&self) -> &profiles::Receiver {
        &self.rx
    }
}

impl From<Profile> for Target<HttpEndpoint> {
    fn from(Profile { target, .. }: Profile) -> Self {
        target
    }
}
