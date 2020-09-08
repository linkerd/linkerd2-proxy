use indexmap::IndexMap;
use linkerd2_app_core::{
    classify, dst, http_request_authority_addr, http_request_host_addr,
    http_request_l5d_override_dst_addr, metric_labels, profiles,
    proxy::{http, identity, tap},
    router, stack_tracing,
    transport::{listen, tls},
    Addr, Conditional, CANONICAL_DST_HEADER, DST_OVERRIDE_HEADER,
};
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::debug;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Target {
    pub dst: Addr,
    pub socket_addr: SocketAddr,
    pub http_settings: http::Settings,
    pub tls_client_id: tls::PeerIdentity,
}

#[derive(Clone, Debug)]
pub struct Logical {
    target: Target,
    profiles: profiles::Receiver,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HttpEndpoint {
    pub port: u16,
    pub settings: http::Settings,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TcpEndpoint {
    pub port: u16,
}

#[derive(Clone, Debug)]
pub struct RequestTarget {
    accept: tls::accept::Meta,
}

#[derive(Copy, Clone, Debug)]
pub struct ProfileTarget;

// === impl HttpEndpoint ===

impl Into<SocketAddr> for HttpEndpoint {
    fn into(self) -> SocketAddr {
        ([127, 0, 0, 1], self.port).into()
    }
}

impl AsRef<http::Settings> for HttpEndpoint {
    fn as_ref(&self) -> &http::Settings {
        &self.settings
    }
}

impl From<Target> for HttpEndpoint {
    fn from(target: Target) -> Self {
        Self {
            port: target.socket_addr.port(),
            settings: target.http_settings,
        }
    }
}

impl tls::HasPeerIdentity for HttpEndpoint {
    fn peer_identity(&self) -> tls::PeerIdentity {
        Conditional::None(tls::ReasonForNoPeerName::Loopback.into())
    }
}

// === TcpEndpoint ===

impl From<listen::Addrs> for TcpEndpoint {
    fn from(addrs: listen::Addrs) -> Self {
        Self {
            port: addrs.target_addr().port(),
        }
    }
}

impl From<tls::accept::Meta> for TcpEndpoint {
    fn from(meta: tls::accept::Meta) -> Self {
        Self {
            port: meta.addrs.target_addr().port(),
        }
    }
}

impl Into<SocketAddr> for TcpEndpoint {
    fn into(self) -> SocketAddr {
        ([127, 0, 0, 1], self.port).into()
    }
}

impl tls::HasPeerIdentity for TcpEndpoint {
    fn peer_identity(&self) -> tls::PeerIdentity {
        Conditional::None(tls::ReasonForNoPeerName::Loopback.into())
    }
}

// === impl Profile ===

pub(super) fn route((route, logical): (profiles::http::Route, Logical)) -> dst::Route {
    dst::Route {
        route,
        target: logical.target.dst,
        direction: metric_labels::Direction::In,
    }
}

// === impl Target ===

impl AsRef<Addr> for Target {
    fn as_ref(&self) -> &Addr {
        &self.dst
    }
}

impl http::normalize_uri::ShouldNormalizeUri for Target {
    fn should_normalize_uri(&self) -> Option<http::uri::Authority> {
        if let http::Settings::Http1 {
            was_absolute_form: false,
            ..
        } = self.http_settings
        {
            return Some(self.dst.to_http_authority());
        }
        None
    }
}

impl AsRef<http::Settings> for Target {
    fn as_ref(&self) -> &http::Settings {
        &self.http_settings
    }
}

impl tls::HasPeerIdentity for Target {
    fn peer_identity(&self) -> tls::PeerIdentity {
        Conditional::None(tls::ReasonForNoPeerName::Loopback.into())
    }
}

impl Into<metric_labels::EndpointLabels> for Target {
    fn into(self) -> metric_labels::EndpointLabels {
        metric_labels::EndpointLabels {
            authority: self.dst.name_addr().map(|d| d.as_http_authority()),
            direction: metric_labels::Direction::In,
            tls_id: self.tls_client_id.map(metric_labels::TlsId::ClientId),
            labels: None,
        }
    }
}

impl classify::CanClassify for Target {
    type Classify = classify::Request;

    fn classify(&self) -> classify::Request {
        classify::Request::default()
    }
}

impl tap::Inspect for Target {
    fn src_addr<B>(&self, req: &http::Request<B>) -> Option<SocketAddr> {
        req.extensions()
            .get::<tls::accept::Meta>()
            .map(|s| s.addrs.peer())
    }

    fn src_tls<'a, B>(
        &self,
        req: &'a http::Request<B>,
    ) -> Conditional<&'a identity::Name, tls::ReasonForNoPeerName> {
        req.extensions()
            .get::<tls::accept::Meta>()
            .map(|s| s.peer_identity.as_ref())
            .unwrap_or_else(|| Conditional::None(tls::ReasonForNoPeerName::LocalIdentityDisabled))
    }

    fn dst_addr<B>(&self, _: &http::Request<B>) -> Option<SocketAddr> {
        Some(self.socket_addr)
    }

    fn dst_labels<B>(&self, _: &http::Request<B>) -> Option<&IndexMap<String, String>> {
        None
    }

    fn dst_tls<B>(
        &self,
        _: &http::Request<B>,
    ) -> Conditional<&identity::Name, tls::ReasonForNoPeerName> {
        Conditional::None(tls::ReasonForNoPeerName::Loopback.into())
    }

    fn route_labels<B>(&self, req: &http::Request<B>) -> Option<Arc<IndexMap<String, String>>> {
        req.extensions()
            .get::<dst::Route>()
            .map(|r| r.route.labels().clone())
    }

    fn is_outbound<B>(&self, _: &http::Request<B>) -> bool {
        false
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.dst.fmt(f)
    }
}

impl stack_tracing::GetSpan<()> for Target {
    fn get_span(&self, _: &()) -> tracing::Span {
        use tracing::info_span;

        match self.http_settings {
            http::Settings::Http2 => match self.dst.name_addr() {
                None => info_span!(
                    "http2",
                    port = %self.socket_addr.port(),
                ),
                Some(name) => info_span!(
                    "http2",
                    %name,
                    port = %self.socket_addr.port(),
                ),
            },
            http::Settings::Http1 {
                keep_alive,
                wants_h1_upgrade,
                was_absolute_form,
            } => match self.dst.name_addr() {
                None => info_span!(
                    "http1",
                    port = %self.socket_addr.port(),
                    keep_alive,
                    wants_h1_upgrade,
                    was_absolute_form,
                ),
                Some(name) => info_span!(
                    "http1",
                    %name,
                    port = %self.socket_addr.port(),
                    keep_alive,
                    wants_h1_upgrade,
                    was_absolute_form,
                ),
            },
        }
    }
}

// === impl RequestTarget ===

impl From<tls::accept::Meta> for RequestTarget {
    fn from(accept: tls::accept::Meta) -> Self {
        Self { accept }
    }
}

impl<A> router::Recognize<http::Request<A>> for RequestTarget {
    type Key = Target;

    fn recognize(&self, req: &http::Request<A>) -> Self::Key {
        let dst = req
            .headers()
            .get(CANONICAL_DST_HEADER)
            .and_then(|dst| {
                dst.to_str().ok().and_then(|d| {
                    Addr::from_str(d).ok().map(|a| {
                        debug!("using {}", CANONICAL_DST_HEADER);
                        a
                    })
                })
            })
            .or_else(|| {
                http_request_l5d_override_dst_addr(req)
                    .ok()
                    .map(|override_addr| {
                        debug!("using {}", DST_OVERRIDE_HEADER);
                        override_addr
                    })
            })
            .or_else(|| http_request_authority_addr(req).ok())
            .or_else(|| http_request_host_addr(req).ok())
            .unwrap_or_else(|| self.accept.addrs.target_addr().into());

        Target {
            dst,
            socket_addr: self.accept.addrs.target_addr(),
            tls_client_id: self.accept.peer_identity.clone(),
            http_settings: http::Settings::from_request(req),
        }
    }
}

impl From<Logical> for Target {
    fn from(Logical { target, .. }: Logical) -> Self {
        target
    }
}

// === impl Logical ===

impl From<(profiles::Receiver, Target)> for Logical {
    fn from((profiles, target): (profiles::Receiver, Target)) -> Self {
        Self { profiles, target }
    }
}

impl AsRef<profiles::Receiver> for Logical {
    fn as_ref(&self) -> &profiles::Receiver {
        &self.profiles
    }
}
