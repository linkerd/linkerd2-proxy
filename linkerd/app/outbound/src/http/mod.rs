pub mod endpoint;
pub mod logical;
mod require_identity_on_endpoint;

#[cfg(test)]
mod tests;

use crate::tcp;
use indexmap::IndexMap;
pub use linkerd_app_core::proxy::http::*;
use linkerd_app_core::{
    dst, identity as id, profiles,
    proxy::{
        api_resolve::ProtocolHint,
        http::{self, CanOverrideAuthority, ClientHandle},
        tap,
    },
    tls, Conditional,
};
use std::{net::SocketAddr, sync::Arc};

pub type Accept = crate::target::Accept<http::Version>;
pub type Logical = crate::target::Logical<http::Version>;
pub type Concrete = crate::target::Concrete<http::Version>;
pub type Endpoint = crate::target::Endpoint<http::Version>;

impl From<(http::Version, tcp::Logical)> for Logical {
    fn from((protocol, logical): (http::Version, tcp::Logical)) -> Self {
        Self {
            protocol,
            orig_dst: logical.orig_dst,
            profile: logical.profile,
        }
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

impl Into<http::client::Settings> for &'_ Endpoint {
    fn into(self) -> http::client::Settings {
        match self.concrete.logical.protocol {
            http::Version::H2 => http::client::Settings::H2,
            http::Version::Http1 => match self.metadata.protocol_hint() {
                ProtocolHint::Unknown => http::client::Settings::Http1,
                ProtocolHint::Http2 => http::client::Settings::OrigProtoUpgrade,
            },
        }
    }
}

// Used to set the l5d-canonical-dst header.
impl From<&'_ Logical> for http::header::HeaderValue {
    fn from(target: &'_ Logical) -> Self {
        http::header::HeaderValue::from_str(&target.addr().to_string())
            .expect("addr must be a valid header")
    }
}

impl CanOverrideAuthority for Endpoint {
    fn override_authority(&self) -> Option<http::uri::Authority> {
        self.metadata.authority_override().cloned()
    }
}

impl tap::Inspect for Endpoint {
    fn src_addr<B>(&self, req: &http::Request<B>) -> Option<SocketAddr> {
        req.extensions().get::<ClientHandle>().map(|c| c.addr)
    }

    fn src_tls<'a, B>(
        &self,
        _: &'a http::Request<B>,
    ) -> Conditional<&'a id::Name, tls::ReasonForNoPeerName> {
        Conditional::None(tls::ReasonForNoPeerName::Loopback)
    }

    fn dst_addr<B>(&self, _: &http::Request<B>) -> Option<SocketAddr> {
        Some(self.addr)
    }

    fn dst_labels<B>(&self, _: &http::Request<B>) -> Option<&IndexMap<String, String>> {
        Some(self.metadata.labels())
    }

    fn dst_tls<B>(&self, _: &http::Request<B>) -> Conditional<&id::Name, tls::ReasonForNoPeerName> {
        self.identity.as_ref().map(|i| i.as_ref())
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
