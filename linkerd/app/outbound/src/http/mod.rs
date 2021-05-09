pub mod detect;
mod endpoint;
pub mod logical;
mod require_identity_on_endpoint;
mod server;

use crate::tcp;
use indexmap::IndexMap;
pub use linkerd_app_core::proxy::http::*;
use linkerd_app_core::{
    dst,
    profiles::{self, LogicalAddr},
    proxy::{api_resolve::ProtocolHint, tap},
    svc::Param,
    tls,
    transport_header::SessionProtocol,
    Addr, Conditional, CANONICAL_DST_HEADER,
};
use std::{net::SocketAddr, str::FromStr, sync::Arc};

pub type Accept = crate::Accept<Version>;
pub type Logical = crate::logical::Logical<Version>;
pub type Concrete = crate::logical::Concrete<Version>;
pub type Endpoint = crate::endpoint::Endpoint<Version>;

#[derive(Clone, Debug)]
pub struct CanonicalDstHeader(pub Addr);

// === impl CanonicalDstHeader ===

impl Into<HeaderPair> for CanonicalDstHeader {
    fn into(self) -> HeaderPair {
        HeaderPair(
            HeaderName::from_static(CANONICAL_DST_HEADER),
            HeaderValue::from_str(&self.0.to_string()).expect("addr must be a valid header"),
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
            orig_dst: logical.orig_dst,
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

impl Logical {
    pub fn mk_route((route, logical): (profiles::http::Route, Self)) -> dst::Route {
        use linkerd_app_core::metrics::Direction;
        dst::Route {
            route,
            target: logical.addr(),
            direction: Direction::Out,
        }
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

impl Param<Option<SessionProtocol>> for Endpoint {
    fn param(&self) -> Option<SessionProtocol> {
        match self.protocol {
            Version::H2 => Some(SessionProtocol::Http2),
            Version::Http1 => match self.metadata.protocol_hint() {
                ProtocolHint::Http2 => Some(SessionProtocol::Http2),
                ProtocolHint::Unknown => Some(SessionProtocol::Http1),
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

    fn dst_labels<B>(&self, _: &Request<B>) -> Option<&IndexMap<String, String>> {
        Some(self.metadata.labels())
    }

    fn dst_tls<B>(&self, _: &Request<B>) -> tls::ConditionalClientTls {
        self.tls.clone()
    }

    fn route_labels<B>(&self, req: &Request<B>) -> Option<Arc<IndexMap<String, String>>> {
        req.extensions()
            .get::<dst::Route>()
            .map(|r| r.route.labels().clone())
    }

    fn is_outbound<B>(&self, _: &Request<B>) -> bool {
        true
    }
}
