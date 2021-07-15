use linkerd_app_core::{
    classify, dst, http_request_authority_addr, http_request_host_addr, identity, metrics,
    profiles,
    proxy::{http, tap},
    stack_tracing,
    svc::{self, Param},
    tls,
    transport::{self, addrs::*, labels},
    transport_header::TransportHeader,
    Addr, Conditional, Error, CANONICAL_DST_HEADER,
};
use std::{convert::TryInto, net::SocketAddr, str::FromStr, sync::Arc};
use tracing::debug;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TcpAccept {
    pub target_addr: SocketAddr,
    pub client_addr: Remote<ClientAddr>,
    pub tls: tls::ConditionalServerTls,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HttpAccept {
    pub tcp: TcpAccept,
    pub version: http::Version,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Target {
    pub dst: Addr,
    pub target_addr: SocketAddr,
    pub http_version: http::Version,
    pub tls: tls::ConditionalServerTls,
}

#[derive(Clone, Debug)]
pub struct Logical {
    target: Target,
    profiles: profiles::Receiver,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HttpEndpoint {
    pub port: u16,
    pub settings: http::client::Settings,
    pub tls: tls::ConditionalServerTls,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TcpEndpoint {
    pub port: u16,
}

#[derive(Clone, Debug)]
pub struct RequestTarget {
    accept: HttpAccept,
}

// === impl TcpAccept ===

impl TcpAccept {
    pub fn port_skipped<T>(tcp: T) -> Self
    where
        T: Param<Remote<ClientAddr>> + Param<OrigDstAddr>,
    {
        let OrigDstAddr(target_addr) = tcp.param();
        Self {
            target_addr,
            client_addr: tcp.param(),
            tls: Conditional::None(tls::NoServerTls::PortSkipped),
        }
    }
}

impl<T> From<tls::server::Meta<T>> for TcpAccept
where
    T: Param<Remote<ClientAddr>> + Param<OrigDstAddr>,
{
    fn from((tls, addrs): tls::server::Meta<T>) -> Self {
        let OrigDstAddr(target_addr) = addrs.param();
        Self {
            target_addr,
            client_addr: addrs.param(),
            tls,
        }
    }
}

impl Param<SocketAddr> for TcpAccept {
    fn param(&self) -> SocketAddr {
        self.target_addr
    }
}

impl Param<OrigDstAddr> for TcpAccept {
    fn param(&self) -> OrigDstAddr {
        OrigDstAddr(self.target_addr)
    }
}

impl Param<transport::labels::Key> for TcpAccept {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::accept(
            transport::labels::Direction::In,
            self.tls.clone(),
            self.target_addr,
        )
    }
}

// === impl HttpAccept ===

impl From<(http::Version, TcpAccept)> for HttpAccept {
    fn from((version, tcp): (http::Version, TcpAccept)) -> Self {
        Self { version, tcp }
    }
}

impl Param<http::Version> for HttpAccept {
    fn param(&self) -> http::Version {
        self.version
    }
}

impl Param<http::normalize_uri::DefaultAuthority> for HttpAccept {
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        http::normalize_uri::DefaultAuthority(Some(
            http::uri::Authority::from_str(&self.tcp.target_addr.to_string())
                .expect("Address must be a valid authority"),
        ))
    }
}

impl Param<Option<identity::Name>> for HttpAccept {
    fn param(&self) -> Option<identity::Name> {
        self.tcp
            .tls
            .value()
            .and_then(|server_tls| match server_tls {
                tls::ServerTls::Established {
                    client_id: Some(id),
                    ..
                } => Some(id.clone().0),
                _ => None,
            })
    }
}

// === impl HttpEndpoint ===

impl Param<http::client::Settings> for HttpEndpoint {
    fn param(&self) -> http::client::Settings {
        self.settings
    }
}

impl From<Target> for HttpEndpoint {
    fn from(target: Target) -> Self {
        Self {
            port: target.target_addr.port(),
            settings: target.http_version.into(),
            tls: target.tls,
        }
    }
}

// === TcpEndpoint ===

impl From<TcpAccept> for TcpEndpoint {
    fn from(tcp: TcpAccept) -> Self {
        Self {
            port: tcp.target_addr.port(),
        }
    }
}

impl From<(TransportHeader, TcpAccept)> for TcpEndpoint {
    fn from((header, _): (TransportHeader, TcpAccept)) -> Self {
        Self { port: header.port }
    }
}

impl From<HttpEndpoint> for TcpEndpoint {
    fn from(h: HttpEndpoint) -> Self {
        Self { port: h.port }
    }
}

impl Param<u16> for TcpEndpoint {
    fn param(&self) -> u16 {
        self.port
    }
}

impl Param<transport::labels::Key> for TcpEndpoint {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::InboundConnect
    }
}

// Needed by `linkerd_app_test::Connect`
#[cfg(test)]
impl From<TcpEndpoint> for SocketAddr {
    fn from(ep: TcpEndpoint) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], ep.port))
    }
}

// === impl Profile ===

pub(super) fn route((route, logical): (profiles::http::Route, Logical)) -> dst::Route {
    dst::Route {
        route,
        target: logical.target.dst,
        direction: metrics::Direction::In,
    }
}

// === impl Target ===

impl From<HttpAccept> for Target {
    fn from(HttpAccept { version, tcp }: HttpAccept) -> Self {
        Self {
            dst: tcp.target_addr.into(),
            target_addr: tcp.target_addr,
            http_version: version,
            tls: tcp.tls,
        }
    }
}

impl From<Logical> for Target {
    fn from(Logical { target, .. }: Logical) -> Self {
        target
    }
}

impl Param<profiles::LookupAddr> for Target {
    fn param(&self) -> profiles::LookupAddr {
        profiles::LookupAddr(self.dst.clone())
    }
}

impl Param<transport::labels::Key> for Target {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::InboundConnect
    }
}

impl Param<metrics::EndpointLabels> for Target {
    fn param(&self) -> metrics::EndpointLabels {
        metrics::InboundEndpointLabels {
            tls: self.tls.clone(),
            authority: self.dst.name_addr().map(|d| d.as_http_authority()),
            target_addr: labels::TargetAddr::from_socket_addr(self.target_addr),
        }
        .into()
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
            .get::<HttpAccept>()
            .map(|s| s.tcp.client_addr.into())
    }

    fn src_tls<B>(&self, req: &http::Request<B>) -> tls::ConditionalServerTls {
        req.extensions()
            .get::<HttpAccept>()
            .map(|s| s.tcp.tls.clone())
            .unwrap_or_else(|| Conditional::None(tls::NoServerTls::Disabled))
    }

    fn dst_addr<B>(&self, _: &http::Request<B>) -> Option<SocketAddr> {
        Some(self.target_addr)
    }

    fn dst_labels<B>(&self, _: &http::Request<B>) -> Option<&tap::Labels> {
        None
    }

    fn dst_tls<B>(&self, _: &http::Request<B>) -> tls::ConditionalClientTls {
        Conditional::None(tls::NoClientTls::Loopback)
    }

    fn route_labels<B>(&self, req: &http::Request<B>) -> Option<Arc<tap::Labels>> {
        req.extensions()
            .get::<dst::Route>()
            .map(|r| r.route.labels().clone())
    }

    fn is_outbound<B>(&self, _: &http::Request<B>) -> bool {
        false
    }
}

impl stack_tracing::GetSpan<()> for Target {
    fn get_span(&self, _: &()) -> tracing::Span {
        use tracing::debug_span;

        match self.http_version {
            http::Version::H2 => match self.dst.name_addr() {
                None => debug_span!("http2"),
                Some(name) => debug_span!("http2", %name),
            },
            http::Version::Http1 => match self.dst.name_addr() {
                None => debug_span!("http1"),
                Some(name) => debug_span!("http1", %name),
            },
        }
    }
}

// === impl RequestTarget ===

impl From<HttpAccept> for RequestTarget {
    fn from(accept: HttpAccept) -> Self {
        Self { accept }
    }
}

impl<A> svc::stack::RecognizeRoute<http::Request<A>> for RequestTarget {
    type Key = Target;

    fn recognize(&self, req: &http::Request<A>) -> Result<Self::Key, Error> {
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
            .or_else(|| http_request_authority_addr(req).ok())
            .or_else(|| http_request_host_addr(req).ok())
            .unwrap_or_else(|| self.accept.tcp.target_addr.into());

        Ok(Target {
            dst,
            target_addr: self.accept.tcp.target_addr,
            tls: self.accept.tcp.tls.clone(),
            // The HttpAccept target version reflects the inbound transport
            // protocol, but it may have changed due to orig-proto downgrading.
            http_version: req
                .version()
                .try_into()
                .expect("HTTP version must be valid"),
        })
    }
}

// === impl Logical ===

impl From<(profiles::Receiver, Target)> for Logical {
    fn from((profiles, target): (profiles::Receiver, Target)) -> Self {
        Self { profiles, target }
    }
}

impl Param<profiles::Receiver> for Logical {
    fn param(&self) -> profiles::Receiver {
        self.profiles.clone()
    }
}
