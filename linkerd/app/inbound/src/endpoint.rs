use http;
use indexmap::IndexMap;
use linkerd2_app_core::{
    classify,
    dst::{DstAddr, Route},
    identity,
    metric_labels::EndpointLabels,
    proxy::http::{router, settings},
    tap,
    transport::{connect, tls},
    Conditional, NameAddr,
};
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::debug;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Endpoint {
    pub addr: SocketAddr,
    pub dst_name: Option<NameAddr>,
    pub http_settings: settings::Settings,
    pub tls_client_id: tls::PeerIdentity,
}

#[derive(Clone, Debug, Default)]
pub struct RecognizeEndpoint {
    _p: (),
}

// === impl Endpoint ===

impl From<SocketAddr> for Endpoint {
    fn from(addr: SocketAddr) -> Self {
        Self {
            addr,
            dst_name: None,
            http_settings: settings::Settings::NotHttp,
            tls_client_id: Conditional::None(tls::ReasonForNoPeerName::NotHttp.into()),
        }
    }
}

impl connect::HasPeerAddr for Endpoint {
    fn peer_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl tls::HasPeerIdentity for Endpoint {
    fn peer_identity(&self) -> tls::PeerIdentity {
        Conditional::None(tls::ReasonForNoPeerName::Loopback.into())
    }
}

impl settings::HasSettings for Endpoint {
    fn http_settings(&self) -> &settings::Settings {
        &self.http_settings
    }
}

impl classify::CanClassify for Endpoint {
    type Classify = classify::Request;

    fn classify(&self) -> classify::Request {
        classify::Request::default()
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
        req: &'a http::Request<B>,
    ) -> Conditional<&'a identity::Name, tls::ReasonForNoIdentity> {
        req.extensions()
            .get::<tls::accept::Meta>()
            .map(|s| s.peer_identity.as_ref())
            .unwrap_or_else(|| Conditional::None(tls::ReasonForNoIdentity::Disabled))
    }

    fn dst_addr<B>(&self, _: &http::Request<B>) -> Option<SocketAddr> {
        Some(self.addr)
    }

    fn dst_labels<B>(&self, _: &http::Request<B>) -> Option<&IndexMap<String, String>> {
        None
    }

    fn dst_tls<B>(
        &self,
        _: &http::Request<B>,
    ) -> Conditional<&identity::Name, tls::ReasonForNoIdentity> {
        Conditional::None(tls::ReasonForNoPeerName::Loopback.into())
    }

    fn route_labels<B>(&self, req: &http::Request<B>) -> Option<Arc<IndexMap<String, String>>> {
        req.extensions().get::<Route>().map(|r| r.labels().clone())
    }

    fn is_outbound<B>(&self, _: &http::Request<B>) -> bool {
        false
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.addr.fmt(f)
    }
}

// === impl RecognizeEndpoint ===

impl<A> router::Recognize<http::Request<A>> for RecognizeEndpoint {
    type Target = Endpoint;

    fn recognize(&self, req: &http::Request<A>) -> Option<Self::Target> {
        let src = req.extensions().get::<tls::accept::Meta>();
        debug!("inbound endpoint: src={:?}", src);
        let addr = src.and_then(|m| m.addrs.target_addr_if_not_local())?;

        let tls_client_id = src
            .map(|s| s.peer_identity.clone())
            .unwrap_or_else(|| Conditional::None(tls::ReasonForNoIdentity::Disabled));

        let dst_addr = req
            .extensions()
            .get::<DstAddr>()
            .expect("request extensions should have DstAddr");

        let dst_name = dst_addr.as_ref().name_addr().cloned();
        let http_settings = dst_addr.http_settings;

        debug!(
            "inbound endpoint: dst={:?}, proto={:?}",
            dst_name, http_settings
        );

        Some(Endpoint {
            addr,
            dst_name,
            http_settings,
            tls_client_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Endpoint, RecognizeEndpoint};
    use http;
    use linkerd2_app_core::{
        proxy::http::{router::Recognize, Settings},
        transport::{listen, tls},
        Conditional,
    };
    use quickcheck::quickcheck;
    use std::net;

    fn make_test_endpoint(addr: net::SocketAddr) -> Endpoint {
        let tls_client_id = TLS_DISABLED;
        Endpoint {
            addr,
            dst_name: None,
            http_settings: Settings::Http2,
            tls_client_id,
        }
    }

    fn dst_addr(req: &mut http::Request<()>) {
        use linkerd2_app_core::{dst::DstAddr, Addr};
        req.extensions_mut().insert(DstAddr::inbound(
            Addr::Socket(([0, 0, 0, 0], 0).into()),
            Settings::Http2,
        ));
    }

    const TLS_DISABLED: tls::PeerIdentity = Conditional::None(tls::ReasonForNoIdentity::Disabled);

    quickcheck! {
        fn recognize_orig_dst(
            orig_dst: net::SocketAddr,
            local: net::SocketAddr,
            peer: net::SocketAddr
        ) -> bool {
            let addrs = listen::Addrs::new(peer, local, Some(orig_dst) ) ;
            let src = tls::accept::Meta { addrs, peer_identity: TLS_DISABLED } ;
            let rec = src.addrs.target_addr_if_not_local().map(make_test_endpoint);

            let mut req = http::Request::new(());
            req.extensions_mut().insert(src);
            dst_addr(&mut req);

            RecognizeEndpoint::default().recognize(&req) == rec
        }
    }
}

impl Into<EndpointLabels> for Endpoint {
    fn into(self) -> EndpointLabels {
        use linkerd2_app_core::metric_labels::{Direction, TlsId};
        EndpointLabels {
            dst_logical: self.dst_name.clone(),
            dst_concrete: self.dst_name,
            direction: Direction::In,
            tls_id: self.tls_client_id.map(TlsId::ClientId),
            labels: None,
        }
    }
}
