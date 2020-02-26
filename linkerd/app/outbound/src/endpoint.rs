use indexmap::IndexMap;
use linkerd2_app_core::{
    dst::{DstAddr, Route},
    metric_labels::{prefix_labels, EndpointLabels},
    proxy::{
        api_resolve::{Metadata, ProtocolHint},
        http::{self, identity_from_header},
        identity,
        resolve::map_endpoint::MapEndpoint,
        tap,
    },
    transport::{connect, tls},
    Addr, Conditional, NameAddr, L5D_REQUIRE_ID,
};
use std::net::SocketAddr;
use std::sync::Arc;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Endpoint {
    pub dst_logical: Option<NameAddr>,
    pub dst_concrete: Option<NameAddr>,
    pub addr: SocketAddr,
    pub identity: tls::PeerIdentity,
    pub metadata: Metadata,
    pub http_settings: http::Settings,
}

#[derive(Copy, Clone, Debug)]
pub struct FromMetadata;

impl Endpoint {
    pub fn can_use_orig_proto(&self) -> bool {
        match self.metadata.protocol_hint() {
            ProtocolHint::Unknown => return false,
            ProtocolHint::Http2 => (),
        }

        match self.http_settings {
            http::Settings::Http2 => false,
            http::Settings::Http1 {
                keep_alive: _,
                wants_h1_upgrade,
                was_absolute_form: _,
            } => !wants_h1_upgrade,
            http::Settings::NotHttp => {
                unreachable!(
                    "Endpoint::can_use_orig_proto called when NotHttp: {:?}",
                    self,
                );
            }
        }
    }

    pub fn from_request<B>(req: &http::Request<B>) -> Option<Self> {
        let addr = req
            .extensions()
            .get::<tls::accept::Meta>()?
            .addrs
            .target_addr_if_not_local()?;

        let http_settings = http::Settings::from_request(req);
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
            http_settings: http::Settings::NotHttp,
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

impl connect::ConnectAddr for Endpoint {
    fn connect_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl http::normalize_uri::ShouldNormalizeUri for Endpoint {
    fn should_normalize_uri(&self) -> Option<http::uri::Authority> {
        if let http::Settings::Http1 {
            was_absolute_form: false,
            ..
        } = self.http_settings
        {
            return Some(
                self.dst_logical
                    .as_ref()
                    .map(|dst| dst.as_http_authority())
                    .unwrap_or_else(|| Addr::from(self.addr).to_http_authority()),
            );
        }
        None
    }
}

impl http::settings::HasSettings for Endpoint {
    fn http_settings(&self) -> &http::Settings {
        &self.http_settings
    }
}

impl tap::Inspect for Endpoint {
    fn src_addr<B>(&self, req: &http::Request<B>) -> Option<SocketAddr> {
        req.extensions()
            .get::<tls::accept::Meta>()
            .map(|s| s.addrs.peer())
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

impl MapEndpoint<DstAddr, Metadata> for FromMetadata {
    type Out = Endpoint;

    fn map_endpoint(&self, target: &DstAddr, addr: SocketAddr, metadata: Metadata) -> Endpoint {
        let identity = metadata
            .identity()
            .cloned()
            .map(Conditional::Some)
            .unwrap_or_else(|| {
                Conditional::None(tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery.into())
            });
        Endpoint {
            addr,
            identity,
            metadata,
            dst_logical: target.dst_logical().name_addr().cloned(),
            dst_concrete: target.dst_concrete().name_addr().cloned(),
            http_settings: target.http_settings.clone(),
        }
    }
}

impl Into<EndpointLabels> for Endpoint {
    fn into(self) -> EndpointLabels {
        use linkerd2_app_core::metric_labels::{Direction, TlsId};
        EndpointLabels {
            dst_logical: self.dst_logical,
            dst_concrete: self.dst_concrete,
            direction: Direction::Out,
            tls_id: self.identity.as_ref().map(|id| TlsId::ServerId(id.clone())),
            labels: prefix_labels("dst", self.metadata.labels().into_iter()),
        }
    }
}
