pub mod endpoint;
pub mod logical;
mod require_identity_on_endpoint;

#[cfg(test)]
mod tests;

use crate::tcp;
use indexmap::IndexMap;
pub use linkerd_app_core::proxy::http::*;
use linkerd_app_core::{
    dst, profiles,
    proxy::{api_resolve::ProtocolHint, tap},
    svc::stack::Param,
    tls,
    transport_header::SessionProtocol,
    Conditional,
};
use std::{net::SocketAddr, str::FromStr, sync::Arc};

pub type Accept = crate::target::Accept<Version>;
pub type Logical = crate::target::Logical<Version>;
pub type Concrete = crate::target::Concrete<Version>;
pub type Endpoint = crate::target::Endpoint<Version>;

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

impl From<(Version, tcp::Logical)> for Logical {
    fn from((protocol, logical): (Version, tcp::Logical)) -> Self {
        Self {
            protocol,
            orig_dst: logical.orig_dst,
            profile: logical.profile,
        }
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
        if let Some(p) = self.profile.as_ref() {
            if let Some(n) = p.borrow().name.as_ref() {
                return normalize_uri::DefaultAuthority(Some(
                    uri::Authority::from_str(&format!("{}:{}", n, self.orig_dst.port()))
                        .expect("Address must be a valid authority"),
                ));
            }
        }

        normalize_uri::DefaultAuthority(Some(
            uri::Authority::from_str(&self.orig_dst.to_string())
                .expect("Address must be a valid authority"),
        ))
    }
}

impl Param<client::Settings> for Endpoint {
    fn param(&self) -> client::Settings {
        match self.logical.protocol {
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
        match self.logical.protocol {
            Version::H2 => Some(SessionProtocol::Http2),
            Version::Http1 => match self.metadata.protocol_hint() {
                ProtocolHint::Http2 => Some(SessionProtocol::Http2),
                ProtocolHint::Unknown => Some(SessionProtocol::Http1),
            },
        }
    }
}

// Used to set the l5d-canonical-dst header.
impl From<&'_ Logical> for header::HeaderValue {
    fn from(target: &'_ Logical) -> Self {
        header::HeaderValue::from_str(&target.addr().to_string())
            .expect("addr must be a valid header")
    }
}

impl CanOverrideAuthority for Endpoint {
    fn override_authority(&self) -> Option<uri::Authority> {
        self.metadata.authority_override().cloned()
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
        Some(self.addr)
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
