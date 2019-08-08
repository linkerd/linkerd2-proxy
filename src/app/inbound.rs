use super::classify;
use super::dst::DstAddr;
use super::identity;
use crate::proxy::http::{router, settings};
use crate::proxy::server::Source;
use crate::tap;
use crate::transport::{connect, tls};
use crate::{Conditional, NameAddr};
use http;
use indexmap::IndexMap;
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
    default_addr: Option<SocketAddr>,
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
        req.extensions().get::<Source>().map(|s| s.remote)
    }

    fn src_tls<'a, B>(
        &self,
        req: &'a http::Request<B>,
    ) -> Conditional<&'a identity::Name, tls::ReasonForNoIdentity> {
        req.extensions()
            .get::<Source>()
            .map(|s| s.tls_peer.as_ref())
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
        req.extensions()
            .get::<super::dst::Route>()
            .map(|r| r.labels().clone())
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

impl RecognizeEndpoint {
    pub fn new(default_addr: Option<SocketAddr>) -> Self {
        Self { default_addr }
    }
}

impl<A> router::Recognize<http::Request<A>> for RecognizeEndpoint {
    type Target = Endpoint;

    fn recognize(&self, req: &http::Request<A>) -> Option<Self::Target> {
        let src = req.extensions().get::<Source>();
        debug!("inbound endpoint: src={:?}", src);
        let addr = src
            .and_then(Source::orig_dst_if_not_local)
            .or(self.default_addr)?;

        let tls_client_id = src
            .map(|s| s.tls_peer.clone())
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

pub mod orig_proto_downgrade {
    use crate::proxy::http::orig_proto;
    use crate::proxy::server::Source;
    use crate::svc;
    use futures::{Future, Poll};
    use http;
    use std::marker::PhantomData;
    use tracing::trace;

    #[derive(Debug)]
    pub struct Layer<A, B>(PhantomData<fn(A) -> B>);

    #[derive(Debug)]
    pub struct Stack<M, A, B> {
        inner: M,
        _marker: PhantomData<fn(A) -> B>,
    }

    // === impl Layer ===

    pub fn layer<A, B>() -> Layer<A, B> {
        Layer(PhantomData)
    }

    impl<A, B> Clone for Layer<A, B> {
        fn clone(&self) -> Self {
            Layer(PhantomData)
        }
    }

    impl<M, A, B> svc::Layer<M> for Layer<A, B>
    where
        M: svc::MakeService<Source, http::Request<A>, Response = http::Response<B>>,
    {
        type Service = Stack<M, A, B>;

        fn layer(&self, inner: M) -> Self::Service {
            Stack {
                inner,
                _marker: PhantomData,
            }
        }
    }

    // === impl Stack ===

    impl<M: Clone, A, B> Clone for Stack<M, A, B> {
        fn clone(&self) -> Self {
            Stack {
                inner: self.inner.clone(),
                _marker: PhantomData,
            }
        }
    }

    impl<M, A, B> svc::Service<Source> for Stack<M, A, B>
    where
        M: svc::MakeService<Source, http::Request<A>, Response = http::Response<B>>,
    {
        type Response = orig_proto::Downgrade<M::Service>;
        type Error = M::MakeError;
        type Future = futures::future::Map<M::Future, fn(M::Service) -> Self::Response>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.inner.poll_ready()
        }

        fn call(&mut self, target: Source) -> Self::Future {
            trace!(
                "supporting {} downgrades for source={:?}",
                orig_proto::L5D_ORIG_PROTO,
                target,
            );
            self.inner
                .make_service(target)
                .map(orig_proto::Downgrade::new)
        }
    }
}

/// Rewrites connect `SocketAddr`s IP address to the loopback address (`127.0.0.1`),
/// with the same port still set.
pub mod rewrite_loopback_addr {
    use super::Endpoint;
    use crate::svc::stack::map_target;
    use std::net::SocketAddr;
    use tracing::debug;

    pub fn layer() -> map_target::Layer<impl Fn(Endpoint) -> Endpoint + Copy> {
        map_target::layer(|mut ep: Endpoint| {
            debug!("rewriting inbound address to loopback; addr={:?}", ep.addr);
            ep.addr = SocketAddr::from(([127, 0, 0, 1], ep.addr.port()));
            ep
        })
    }
}

/// Adds `l5d-client-id` headers to http::Requests derived from the
/// TlsIdentity of a `Source`.
#[allow(dead_code)] // TODO #2597
pub mod set_client_id_on_req {
    use super::super::L5D_CLIENT_ID;
    use crate::proxy::{
        http::add_header::{self, request::ReqHeader, Layer},
        server::Source,
    };
    use crate::Conditional;
    use http::header::HeaderValue;
    use tracing::{debug, warn};

    pub fn layer() -> Layer<&'static str, Source, ReqHeader> {
        add_header::request::layer(L5D_CLIENT_ID, |source: &Source| {
            if let Conditional::Some(ref id) = source.tls_peer {
                match HeaderValue::from_str(id.as_ref()) {
                    Ok(value) => {
                        debug!("l5d-client-id enabled for {:?}", source);
                        return Some(value);
                    }
                    Err(_err) => {
                        warn!("l5d-client-id identity header is invalid: {:?}", source);
                    }
                };
            }

            None
        })
    }
}

/// Adds `l5d-remote-ip` headers to http::Requests derived from the
/// `remote` of a `Source`.
#[allow(dead_code)] // TODO #2597
pub mod set_remote_ip_on_req {
    use super::super::L5D_REMOTE_IP;
    use crate::proxy::{
        http::add_header::{self, request::ReqHeader, Layer},
        server::Source,
    };
    use bytes::Bytes;
    use http::header::HeaderValue;

    pub fn layer() -> Layer<&'static str, Source, ReqHeader> {
        add_header::request::layer(L5D_REMOTE_IP, |source: &Source| {
            HeaderValue::from_shared(Bytes::from(source.remote.ip().to_string())).ok()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Endpoint, RecognizeEndpoint};
    use crate::proxy::http::{router::Recognize, Settings};
    use crate::proxy::server::Source;
    use crate::transport::tls;
    use crate::Conditional;
    use http;
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
        use crate::app::dst::DstAddr;
        use crate::Addr;
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
            remote: net::SocketAddr
        ) -> bool {
            let src = Source::for_test(remote, local, Some(orig_dst), TLS_DISABLED);
            let rec = src.orig_dst_if_not_local().map(make_test_endpoint);

            let mut req = http::Request::new(());
            req.extensions_mut().insert(src);
            dst_addr(&mut req);

            RecognizeEndpoint::default().recognize(&req) == rec
        }

        fn recognize_default_no_orig_dst(
            default: Option<net::SocketAddr>,
            local: net::SocketAddr,
            remote: net::SocketAddr
        ) -> bool {
            let mut req = http::Request::new(());
            req.extensions_mut()
                .insert(Source::for_test(remote, local, None, TLS_DISABLED));
            dst_addr(&mut req);

            RecognizeEndpoint::new(default).recognize(&req) == default.map(make_test_endpoint)
        }

        fn recognize_default_no_ctx(default: Option<net::SocketAddr>) -> bool {
            let mut req = http::Request::new(());
            dst_addr(&mut req);
            RecognizeEndpoint::new(default).recognize(&req) == default.map(make_test_endpoint)
        }

        fn recognize_default_no_loop(
            default: Option<net::SocketAddr>,
            local: net::SocketAddr,
            remote: net::SocketAddr
        ) -> bool {
            let mut req = http::Request::new(());
            req.extensions_mut()
                .insert(Source::for_test(remote, local, Some(local), TLS_DISABLED));
            dst_addr(&mut req);

            RecognizeEndpoint::new(default).recognize(&req) == default.map(make_test_endpoint)
        }
    }
}
