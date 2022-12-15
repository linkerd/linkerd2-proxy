pub mod detect;
mod endpoint;
pub mod logical;
mod proxy_connection_close;
mod require_id_header;
mod retry;
mod server;
mod strip_proxy_error;

use self::{
    proxy_connection_close::ProxyConnectionClose, require_id_header::NewRequireIdentity,
    strip_proxy_error::NewStripProxyError,
};
pub(crate) use self::{require_id_header::IdentityRequired, server::ServerRescue};
use crate::{policy, tcp};
pub use linkerd_app_core::proxy::http::*;
use linkerd_app_core::{
    classify, metrics,
    profiles::LogicalAddr,
    proxy::{api_resolve::ProtocolHint, tap},
    svc::Param,
    tls, Addr, Conditional, CANONICAL_DST_HEADER,
};
use std::{net::SocketAddr, str::FromStr};

pub type Accept = crate::Accept<Version>;
pub type Logical = crate::logical::Logical<Version>;
pub type Concrete = crate::logical::Concrete<Version>;
pub type Endpoint = crate::endpoint::Endpoint<Version>;

pub type Connect = self::endpoint::Connect<Endpoint>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PolicyRoute {
    logical: Logical,
    route: policy::http::RoutePolicy,
}

#[derive(Clone, Debug)]
pub struct CanonicalDstHeader(pub Addr);

// === impl CanonicalDstHeader ===

impl From<CanonicalDstHeader> for HeaderPair {
    fn from(CanonicalDstHeader(dst): CanonicalDstHeader) -> HeaderPair {
        HeaderPair(
            HeaderName::from_static(CANONICAL_DST_HEADER),
            HeaderValue::from_str(&dst.to_string()).expect("addr must be a valid header"),
        )
    }
}

// === impl Accept ===

impl Param<Version> for Accept {
    fn param(&self) -> Version {
        self.protocol
    }
}

impl Param<normalize_uri::DefaultAuthority> for Accept {
    fn param(&self) -> normalize_uri::DefaultAuthority {
        normalize_uri::DefaultAuthority(Some(
            uri::Authority::from_str(&self.orig_dst.to_string())
                .expect("Address must be a valid authority"),
        ))
    }
}

// === impl Logical ===

impl From<(Version, tcp::Logical)> for Logical {
    fn from((protocol, logical): (Version, tcp::Logical)) -> Self {
        Self {
            protocol,
            policy: logical.policy,
            profile: logical.profile,
            logical_addr: logical.logical_addr,
        }
    }
}

impl Param<CanonicalDstHeader> for Logical {
    fn param(&self) -> CanonicalDstHeader {
        CanonicalDstHeader(self.addr())
    }
}

impl Param<Version> for Logical {
    fn param(&self) -> Version {
        self.protocol
    }
}

impl Param<normalize_uri::DefaultAuthority> for Logical {
    fn param(&self) -> normalize_uri::DefaultAuthority {
        normalize_uri::DefaultAuthority(Some(
            uri::Authority::from_str(&self.logical_addr.to_string())
                .expect("Address must be a valid authority"),
        ))
    }
}

impl Param<policy::http::RouteListStream> for Logical {
    fn param(&self) -> policy::http::RouteListStream {
        use async_stream::stream;
        use policy::http::RouteList;

        // If a client policy was discovered, use its HTTP routes.
        if let Some(mut rx) = self.policy.clone() {
            return Box::pin(stream! {
                loop {
                    let routes = rx.borrow_and_update().http_routes.clone();
                    yield RouteList::HttpRoute(routes);
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
            });
        }

        // otherwise, use service profile HTTP routes (if there are any).
        let mut rx = self.profile.clone().into_inner();
        Box::pin(stream! {
            loop {
                let routes = rx.borrow_and_update().http_routes.clone();
                yield RouteList::Profile(routes);
                if rx.changed().await.is_err() {
                    break;
                }
            }
        })
    }
}

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

impl Param<normalize_uri::DefaultAuthority> for Endpoint {
    fn param(&self) -> normalize_uri::DefaultAuthority {
        if let Some(LogicalAddr(ref a)) = self.logical_addr {
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

impl Param<Version> for Endpoint {
    fn param(&self) -> Version {
        self.protocol
    }
}

impl Param<client::Settings> for Endpoint {
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
            .get::<policy::http::RoutePolicy>()
            .map(|r| r.labels().clone())
    }

    fn is_outbound<B>(&self, _: &Request<B>) -> bool {
        true
    }
}

// === impl PolicyRoute ===

impl Param<policy::http::RoutePolicy> for PolicyRoute {
    fn param(&self) -> policy::http::RoutePolicy {
        self.route.clone()
    }
}

impl Param<metrics::ProfileRouteLabels> for PolicyRoute {
    fn param(&self) -> metrics::ProfileRouteLabels {
        metrics::ProfileRouteLabels::outbound(self.logical.logical_addr.clone(), &self.route)
    }
}

impl Param<ResponseTimeout> for PolicyRoute {
    fn param(&self) -> ResponseTimeout {
        ResponseTimeout(self.route.timeout())
    }
}

impl classify::CanClassify for PolicyRoute {
    type Classify = classify::Request;

    fn classify(&self) -> classify::Request {
        self.route.response_classes().clone().into()
    }
}

// === impl ProfileRoute ===

impl Param<Vec<policy::split::Backend>> for PolicyRoute {
    fn param(&self) -> Vec<policy::split::Backend> {
        self.route.backends.clone()
    }
}

impl Param<LogicalAddr> for PolicyRoute {
    fn param(&self) -> LogicalAddr {
        self.logical.logical_addr.clone()
    }
}
