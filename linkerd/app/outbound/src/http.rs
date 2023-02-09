use self::{
    proxy_connection_close::ProxyConnectionClose, require_id_header::NewRequireIdentity,
    strip_proxy_error::NewStripProxyError,
};
use crate::{tcp, Outbound};
use linkerd_app_core::{
    io, profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata, ProtocolHint},
        core::Resolve,
        tap,
    },
    svc, tls,
    transport::addrs::*,
    Addr, Conditional, Error, CANONICAL_DST_HEADER,
};
use std::{fmt::Debug, hash::Hash, net::SocketAddr, str::FromStr};

pub mod concrete;
pub mod detect;
mod endpoint;
pub mod logical;
mod proxy_connection_close;
mod require_id_header;
mod retry;
mod server;
mod strip_proxy_error;

pub(crate) use self::require_id_header::IdentityRequired;
pub use linkerd_app_core::proxy::http::{self as http, *};

pub type Endpoint = crate::endpoint::Endpoint<Version>;

pub type Connect = self::endpoint::Connect<Endpoint>;

#[derive(Clone, Debug)]
struct CanonicalDstHeader(pub Addr);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Http {
    target: logical::Target,
    version: http::Version,
}

// === impl Outbound ===

impl<C> Outbound<C> {
    /// Builds a stack that handles protocol detection as well as routing and
    /// load balancing for a single logical destination.
    ///
    /// This stack uses caching so that a router/load-balancer may be reused
    /// across multiple connections.
    pub fn push_http<T, R>(
        self,
        resolve: R,
    ) -> Outbound<
        svc::ArcNewService<
            T,
            impl svc::Service<
                    http::Request<http::BoxBody>,
                    Response = http::Response<http::BoxBody>,
                    Error = Error,
                    Future = impl Send,
                > + Clone,
        >,
    >
    where
        T: svc::Param<http::Version>,
        T: svc::Param<logical::Target>,
        T: Clone + Send + Sync + 'static,
        C: svc::MakeConnection<tcp::Connect, Metadata = Local<ClientAddr>, Error = io::Error>,
        C: Clone + Send + Sync + Unpin + 'static,
        C::Connection: Send + Unpin,
        C::Future: Send,
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
    {
        self.push_tcp_endpoint()
            .push_http_endpoint()
            .push_http_concrete(resolve)
            .push_http_logical()
            .push_http_server()
            .map_stack(move |_, _, stk| {
                stk.push_map_target(Http::new)
                    .push(svc::ArcNewService::layer())
                    .check_new_service::<T, http::Request<http::BoxBody>>()
            })
    }
}

// === impl Http ===

impl Http {
    pub fn new<T>(parent: T) -> Self
    where
        T: svc::Param<logical::Target>,
        T: svc::Param<http::Version>,
    {
        Self {
            target: parent.param(),
            version: parent.param(),
        }
    }
}

impl svc::Param<http::Version> for Http {
    fn param(&self) -> http::Version {
        self.version
    }
}

impl svc::Param<logical::Target> for Http {
    fn param(&self) -> logical::Target {
        self.target.clone()
    }
}

impl svc::Param<http::normalize_uri::DefaultAuthority> for Http {
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        let addr = match self.target.param() {
            logical::Target::Route(addr, _) => Addr::from(addr),
            logical::Target::Forward(Remote(ServerAddr(addr)), _) => Addr::from(addr),
        };

        http::normalize_uri::DefaultAuthority(Some(addr.to_http_authority()))
    }
}

impl svc::Param<Option<profiles::LogicalAddr>> for Http {
    fn param(&self) -> Option<profiles::LogicalAddr> {
        match self.target {
            logical::Target::Route(ref addr, _) => Some(profiles::LogicalAddr(addr.clone())),
            logical::Target::Forward(_, _) => None,
        }
    }
}

impl svc::Param<Option<profiles::Receiver>> for Http {
    fn param(&self) -> Option<profiles::Receiver> {
        match self.target {
            logical::Target::Route(_, ref rx) => Some(rx.clone()),
            logical::Target::Forward(_, _) => None,
        }
    }
}

// === impl CanonicalDstHeader ===

impl From<CanonicalDstHeader> for HeaderPair {
    fn from(CanonicalDstHeader(dst): CanonicalDstHeader) -> HeaderPair {
        HeaderPair(
            HeaderName::from_static(CANONICAL_DST_HEADER),
            HeaderValue::from_str(&dst.to_string()).expect("addr must be a valid header"),
        )
    }
}

// === impl Logical ===

/*
impl svc::Param<CanonicalDstHeader> for Logical {
    fn param(&self) -> CanonicalDstHeader {
        CanonicalDstHeader(self.addr())
    }
}

impl svc::Param<Version> for Logical {
    fn param(&self) -> Version {
        self.protocol
    }
}

impl svc::Param<Option<Version>> for Logical {
    fn param(&self) -> Option<Version> {
        Some(self.protocol)
    }
}

impl svc::Param<normalize_uri::DefaultAuthority> for Logical {
    fn param(&self) -> normalize_uri::DefaultAuthority {
        normalize_uri::DefaultAuthority(Some(
            uri::Authority::from_str(&self.logical_addr.to_string())
                .expect("Address must be a valid authority"),
        ))
    }
}
*/

// === impl Endpoint ===

impl From<(Version, tcp::Endpoint)> for Endpoint {
    fn from((protocol, ep): (Version, tcp::Endpoint)) -> Self {
        Self {
            protocol,
            addr: ep.addr,
            tls: ep.tls,
            metadata: ep.metadata,
            logical_addr: ep.logical_addr,
            // If we know an HTTP version, the protocol must not be opaque.
            opaque_protocol: false,
        }
    }
}

impl svc::Param<normalize_uri::DefaultAuthority> for Endpoint {
    fn param(&self) -> normalize_uri::DefaultAuthority {
        if let Some(profiles::LogicalAddr(ref a)) = self.logical_addr {
            normalize_uri::DefaultAuthority(Some(
                uri::Authority::from_str(&a.to_string())
                    .expect("Address must be a valid authority"),
            ))
        } else {
            normalize_uri::DefaultAuthority(Some(
                uri::Authority::from_str(&self.addr.to_string())
                    .expect("Address must be a valid authority"),
            ))
        }
    }
}

impl svc::Param<Version> for Endpoint {
    fn param(&self) -> Version {
        self.protocol
    }
}

impl svc::Param<client::Settings> for Endpoint {
    fn param(&self) -> client::Settings {
        match self.protocol {
            Version::H2 => client::Settings::H2,
            Version::Http1 => match self.metadata.protocol_hint() {
                ProtocolHint::Unknown => client::Settings::Http1,
                ProtocolHint::Http2 => client::Settings::OrigProtoUpgrade,
            },
        }
    }
}

impl tap::Inspect for Endpoint {
    fn src_addr<B>(&self, req: &Request<B>) -> Option<SocketAddr> {
        req.extensions().get::<ClientHandle>().map(|c| c.addr)
    }

    fn src_tls<B>(&self, _: &Request<B>) -> tls::ConditionalServerTls {
        Conditional::None(tls::NoServerTls::Loopback)
    }

    fn dst_addr<B>(&self, _: &Request<B>) -> Option<SocketAddr> {
        Some(self.addr.into())
    }

    fn dst_labels<B>(&self, _: &Request<B>) -> Option<tap::Labels> {
        Some(self.metadata.labels())
    }

    fn dst_tls<B>(&self, _: &Request<B>) -> tls::ConditionalClientTls {
        self.tls.clone()
    }

    fn route_labels<B>(&self, req: &Request<B>) -> Option<tap::Labels> {
        req.extensions()
            .get::<profiles::http::Route>()
            .map(|r| r.labels().clone())
    }

    fn is_outbound<B>(&self, _: &Request<B>) -> bool {
        true
    }
}
