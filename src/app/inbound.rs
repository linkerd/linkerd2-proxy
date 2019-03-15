use http;
use indexmap::IndexMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;

use super::classify;
use super::dst::DstAddr;
use super::identity;
use proxy::http::{router};
use proxy::server::Source;
use tap;
use transport::{connect, tls};
use {Conditional, NameAddr};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Endpoint {
    pub addr: SocketAddr,
    pub dst_name: Option<NameAddr>,
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
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
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

        let dst_name = req
            .extensions()
            .get::<DstAddr>()
            .and_then(|a| a.as_ref().name_addr())
            .cloned();
        debug!("inbound endpoint: dst={:?}", dst_name);

        Some(Endpoint {
            addr,
            dst_name,
            tls_client_id,
        })
    }
}

pub mod orig_proto_downgrade {
    use http;
    use proxy::http::orig_proto;
    use proxy::server::Source;
    use std::marker::PhantomData;
    use svc;

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

    impl<M, A, B> svc::Layer<Source, Source, M> for Layer<A, B>
    where
        M: svc::Stack<Source>,
        M::Value: svc::Service<http::Request<A>, Response = http::Response<B>>,
    {
        type Value = <Stack<M, A, B> as svc::Stack<Source>>::Value;
        type Error = <Stack<M, A, B> as svc::Stack<Source>>::Error;
        type Stack = Stack<M, A, B>;

        fn bind(&self, inner: M) -> Self::Stack {
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

    impl<M, A, B> svc::Stack<Source> for Stack<M, A, B>
    where
        M: svc::Stack<Source>,
        M::Value: svc::Service<http::Request<A>, Response = http::Response<B>>,
    {
        type Value = orig_proto::Downgrade<M::Value>;
        type Error = M::Error;

        fn make(&self, target: &Source) -> Result<Self::Value, Self::Error> {
            trace!(
                "supporting {} downgrades for source={:?}",
                orig_proto::L5D_ORIG_PROTO,
                target,
            );
            self.inner.make(&target).map(orig_proto::Downgrade::new)
        }
    }
}

/// Rewrites connect `SocketAddr`s IP address to the loopback address (`127.0.0.1`),
/// with the same port still set.
pub mod rewrite_loopback_addr {
    use std::net::SocketAddr;
    use svc;

    #[derive(Debug, Clone)]
    pub struct Layer;

    #[derive(Clone, Debug)]
    pub struct Stack<M>
    where
        M: svc::Stack<super::Endpoint>,
    {
        inner: M,
    }

    // === impl Layer ===

    pub fn layer() -> Layer {
        Layer
    }

    impl<M> svc::Layer<super::Endpoint, super::Endpoint, M> for Layer
    where
        M: svc::Stack<super::Endpoint>,
    {
        type Value = <Stack<M> as svc::Stack<super::Endpoint>>::Value;
        type Error = <Stack<M> as svc::Stack<super::Endpoint>>::Error;
        type Stack = Stack<M>;

        fn bind(&self, inner: M) -> Self::Stack {
            Stack { inner }
        }
    }

    // === impl Stack ===

    impl<M> svc::Stack<super::Endpoint> for Stack<M>
    where
        M: svc::Stack<super::Endpoint>,
    {
        type Value = M::Value;
        type Error = M::Error;

        fn make(&self, ep: &super::Endpoint) -> Result<Self::Value, Self::Error> {
            debug!("rewriting inbound address to loopback; addr={:?}", ep.addr);

            let mut ep = ep.clone();
            ep.addr = SocketAddr::from(([127, 0, 0, 1], ep.addr.port()));

            self.inner.make(&ep)
        }
    }
}

/// Adds `l5d-client-id` headers to http::Requests derived from the
/// TlsIdentity of a `Source`.
pub mod set_client_id_on_req {
    use super::super::L5D_CLIENT_ID;
    use http::header::HeaderValue;

    use proxy::{
        http::add_header::{self, request::ReqHeader, Layer},
        server::Source,
    };
    use Conditional;

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
pub mod set_remote_ip_on_req {
    use super::super::L5D_REMOTE_IP;
    use bytes::Bytes;
    use http::header::HeaderValue;
    use proxy::{
        http::add_header::{self, request::ReqHeader, Layer},
        server::Source,
    };

    pub fn layer() -> Layer<&'static str, Source, ReqHeader> {
        add_header::request::layer(L5D_REMOTE_IP, |source: &Source| {
            HeaderValue::from_shared(Bytes::from(source.remote.ip().to_string())).ok()
        })
    }
}

#[cfg(test)]
mod tests {
    use http;
    use std::net;

    use super::{Endpoint, RecognizeEndpoint};
    use proxy::http::router::Recognize;
    use proxy::server::Source;
    use transport::tls;
    use Conditional;

    fn make_h1_endpoint(addr: net::SocketAddr) -> Endpoint {
        let tls_client_id = TLS_DISABLED;
        Endpoint {
            addr,
            dst_name: None,
            tls_client_id,
        }
    }

    const TLS_DISABLED: tls::PeerIdentity = Conditional::None(tls::ReasonForNoIdentity::Disabled);

    quickcheck! {
        fn recognize_orig_dst(
            orig_dst: net::SocketAddr,
            local: net::SocketAddr,
            remote: net::SocketAddr
        ) -> bool {
            let src = Source::for_test(remote, local, Some(orig_dst), TLS_DISABLED);
            let rec = src.orig_dst_if_not_local().map(make_h1_endpoint);

            let mut req = http::Request::new(());
            req.extensions_mut().insert(src);

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

            RecognizeEndpoint::new(default).recognize(&req) == default.map(make_h1_endpoint)
        }

        fn recognize_default_no_ctx(default: Option<net::SocketAddr>) -> bool {
            let req = http::Request::new(());
            RecognizeEndpoint::new(default).recognize(&req) == default.map(make_h1_endpoint)
        }

        fn recognize_default_no_loop(
            default: Option<net::SocketAddr>,
            local: net::SocketAddr,
            remote: net::SocketAddr
        ) -> bool {
            let mut req = http::Request::new(());
            req.extensions_mut()
                .insert(Source::for_test(remote, local, Some(local), TLS_DISABLED));

            RecognizeEndpoint::new(default).recognize(&req) == default.map(make_h1_endpoint)
        }
    }
}
