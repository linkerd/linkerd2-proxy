mod concrete;
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
use crate::tcp;
pub use linkerd_app_core::proxy::http::*;
use linkerd_app_core::{
    profiles::{self, LogicalAddr},
    proxy::{api_resolve::ProtocolHint, tap},
    svc, tls, Addr, Conditional, CANONICAL_DST_HEADER,
};
use std::{net::SocketAddr, str::FromStr};

pub type Logical = crate::logical::Logical<Version>;
pub type Concrete = crate::logical::Concrete<Version>;
pub type Endpoint = crate::endpoint::Endpoint<Version>;

pub type Connect = self::endpoint::Connect<Endpoint>;

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

// === impl Logical ===

impl From<(Version, tcp::Logical)> for Logical {
    fn from((protocol, logical): (Version, tcp::Logical)) -> Self {
        Self {
            protocol,
            profile: logical.profile,
            logical_addr: logical.logical_addr,
        }
    }
}

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
