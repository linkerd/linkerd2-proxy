use super::super::{dst::Route, L5D_REQUIRE_ID};
use crate::control::destination::{Metadata, ProtocolHint};
use crate::proxy::http::{identity_from_header, settings};
use crate::proxy::Source;
use crate::transport::{connect, tls};
use crate::{identity, tap};
use crate::{Conditional, NameAddr};
use indexmap::IndexMap;
use std::net::SocketAddr;
use std::sync::Arc;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Endpoint {
    pub dst_logical: Option<NameAddr>,
    pub dst_concrete: Option<NameAddr>,
    pub addr: SocketAddr,
    pub identity: tls::PeerIdentity,
    pub metadata: Metadata,
    pub http_settings: settings::Settings,
}

impl Endpoint {
    pub fn can_use_orig_proto(&self) -> bool {
        match self.metadata.protocol_hint() {
            ProtocolHint::Unknown => return false,
            ProtocolHint::Http2 => (),
        }

        match self.http_settings {
            settings::Settings::Http2 => false,
            settings::Settings::Http1 {
                keep_alive: _,
                wants_h1_upgrade,
                was_absolute_form: _,
            } => !wants_h1_upgrade,
            settings::Settings::NotHttp => {
                unreachable!(
                    "Endpoint::can_use_orig_proto called when NotHttp: {:?}",
                    self,
                );
            }
        }
    }

    pub fn from_request<B>(req: &http::Request<B>) -> Option<Self> {
        let addr = req.extensions().get::<Source>()?.orig_dst_if_not_local()?;
        let http_settings = settings::Settings::from_request(req);
        let identity = match identity_from_header(req, L5D_REQUIRE_ID) {
            Some(require_id) => Conditional::Some(require_id),
            None => {
                Conditional::None(tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery.into())
            }
        };

        Some(Self {
            addr,
            dst_logical: None,
            dst_concrete: None,
            identity,
            metadata: Metadata::empty(),
            http_settings,
        })
    }
}

impl From<SocketAddr> for Endpoint {
    fn from(addr: SocketAddr) -> Self {
        Self {
            addr,
            dst_logical: None,
            dst_concrete: None,
            identity: Conditional::None(tls::ReasonForNoPeerName::NotHttp.into()),
            metadata: Metadata::empty(),
            http_settings: settings::Settings::NotHttp,
        }
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.addr.fmt(f)
    }
}

impl std::hash::Hash for Endpoint {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.dst_logical.hash(state);
        self.dst_concrete.hash(state);
        self.addr.hash(state);
        self.identity.hash(state);
        self.http_settings.hash(state);
        // Ignore metadata.
    }
}

impl tls::HasPeerIdentity for Endpoint {
    fn peer_identity(&self) -> tls::PeerIdentity {
        self.identity.clone()
    }
}

impl connect::HasPeerAddr for Endpoint {
    fn peer_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl settings::HasSettings for Endpoint {
    fn http_settings(&self) -> &settings::Settings {
        &self.http_settings
    }
}

impl tap::Inspect for Endpoint {
    fn src_addr<B>(&self, req: &http::Request<B>) -> Option<SocketAddr> {
        req.extensions().get::<Source>().map(|s| s.remote)
    }

    fn src_tls<'a, B>(
        &self,
        _: &'a http::Request<B>,
    ) -> Conditional<&'a identity::Name, tls::ReasonForNoIdentity> {
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
    ) -> Conditional<&identity::Name, tls::ReasonForNoIdentity> {
        self.identity.as_ref()
    }

    fn route_labels<B>(&self, req: &http::Request<B>) -> Option<Arc<IndexMap<String, String>>> {
        req.extensions().get::<Route>().map(|r| r.labels().clone())
    }

    fn is_outbound<B>(&self, _: &http::Request<B>) -> bool {
        true
    }
}
