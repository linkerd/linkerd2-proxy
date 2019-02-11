use http;
use indexmap::IndexMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;

use super::classify;
use super::dst::DstAddr;
use proxy::http::{router, settings};
use proxy::server::Source;
use tap;
use transport::{connect, tls};
use {Conditional, NameAddr};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Endpoint {
    pub addr: SocketAddr,
    pub dst_name: Option<NameAddr>,
    pub tls_client_id: tls::ConditionalIdentity,
}

#[derive(Clone, Debug, Default)]
pub struct RecognizeEndpoint {
    default_addr: Option<SocketAddr>,
}

// === impl Endpoint ===

impl classify::CanClassify for Endpoint {
    type Classify = classify::Request;

    fn classify(&self) -> classify::Request {
        classify::Request::default()
    }
}

impl Endpoint {
    fn target(&self) -> connect::Target {
        let tls = Conditional::None(tls::ReasonForNoTls::InternalTraffic);
        connect::Target::new(self.addr, tls)
    }
}

impl settings::router::HasConnect for Endpoint {
    fn connect(&self) -> connect::Target {
        self.target()
    }
}

impl tap::Inspect for Endpoint {
    fn src_addr<B>(&self, req: &http::Request<B>) -> Option<SocketAddr> {
        req.extensions().get::<Source>().map(|s| s.remote)
    }

    fn src_tls<'a, B>(
        &self,
        req: &'a http::Request<B>,
    ) -> Conditional<&'a tls::Identity, tls::ReasonForNoTls> {
        req.extensions()
            .get::<Source>()
            .map(|s| s.tls_peer.as_ref())
            .unwrap_or_else(|| Conditional::None(tls::ReasonForNoTls::Disabled))
    }

    fn dst_addr<B>(&self, _: &http::Request<B>) -> Option<SocketAddr> {
        Some(self.addr)
    }

    fn dst_labels<B>(&self, _: &http::Request<B>) -> Option<&IndexMap<String, String>> {
        None
    }

    fn dst_tls<B>(&self, _: &http::Request<B>) -> Conditional<&tls::Identity, tls::ReasonForNoTls> {
        Conditional::None(tls::ReasonForNoTls::InternalTraffic)
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
            .unwrap_or_else(|| Conditional::None(tls::ReasonForNoTls::Disabled));

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

/// Rewrites connect `Target`s IP address to the loopback address (`127.0.0.1`),
/// with the same port still set.
pub mod rewrite_loopback_addr {
    use std::net::SocketAddr;
    use svc;
    use transport::connect::Target;

    #[derive(Debug, Clone)]
    pub struct Layer;

    #[derive(Clone, Debug)]
    pub struct Stack<M>
    where
        M: svc::Stack<Target>,
    {
        inner: M,
    }

    // === impl Layer ===

    pub fn layer() -> Layer {
        Layer
    }

    impl<M> svc::Layer<Target, Target, M> for Layer
    where
        M: svc::Stack<Target>,
    {
        type Value = <Stack<M> as svc::Stack<Target>>::Value;
        type Error = <Stack<M> as svc::Stack<Target>>::Error;
        type Stack = Stack<M>;

        fn bind(&self, inner: M) -> Self::Stack {
            Stack { inner }
        }
    }

    // === impl Stack ===

    impl<M> svc::Stack<Target> for Stack<M>
    where
        M: svc::Stack<Target>,
    {
        type Value = M::Value;
        type Error = M::Error;

        fn make(&self, target: &Target) -> Result<Self::Value, Self::Error> {
            debug!("rewriting inbound address to loopback; target={:?}", target);

            let rewritten = SocketAddr::from(([127, 0, 0, 1], target.addr.port()));
            let target = Target::new(rewritten, target.tls.clone());
            self.inner.make(&target)
        }
    }
}

/// Adds `l5d-client-id` headers to http::Requests derived from the
/// TlsIdentity of a `Source`.
pub mod client_id {
    use std::marker::PhantomData;

    use futures::Poll;
    use http::{self, header::HeaderValue};

    use proxy::server::Source;
    use svc;
    use Conditional;

    #[derive(Debug)]
    pub struct Layer<B>(PhantomData<fn() -> B>);

    #[derive(Debug)]
    pub struct Stack<M, B> {
        inner: M,
        _marker: PhantomData<fn() -> B>,
    }

    #[derive(Debug)]
    pub struct Service<S, B> {
        inner: S,
        value: HeaderValue,
        _marker: PhantomData<fn() -> B>,
    }

    pub fn layer<B>() -> Layer<B> {
        Layer(PhantomData)
    }

    impl<B> Clone for Layer<B> {
        fn clone(&self) -> Self {
            Layer(PhantomData)
        }
    }

    impl<M, B> svc::Layer<Source, Source, M> for Layer<B>
    where
        M: svc::Stack<Source>,
    {
        type Value = <Stack<M, B> as svc::Stack<Source>>::Value;
        type Error = <Stack<M, B> as svc::Stack<Source>>::Error;
        type Stack = Stack<M, B>;

        fn bind(&self, inner: M) -> Self::Stack {
            Stack {
                inner,
                _marker: PhantomData,
            }
        }
    }

    // === impl Stack ===

    impl<M: Clone, B> Clone for Stack<M, B> {
        fn clone(&self) -> Self {
            Stack {
                inner: self.inner.clone(),
                _marker: PhantomData,
            }
        }
    }

    impl<M, B> svc::Stack<Source> for Stack<M, B>
    where
        M: svc::Stack<Source>,
    {
        type Value = svc::Either<Service<M::Value, B>, M::Value>;
        type Error = M::Error;

        fn make(&self, source: &Source) -> Result<Self::Value, Self::Error> {
            let svc = self.inner.make(source)?;

            if let Conditional::Some(ref id) = source.tls_peer {
                match HeaderValue::from_str(id.as_ref()) {
                    Ok(value) => {
                        debug!("l5d-client-id enabled for {:?}", source);
                        return Ok(svc::Either::A(Service {
                            inner: svc,
                            value,
                            _marker: PhantomData,
                        }));
                    }
                    Err(_err) => {
                        warn!("l5d-client-id identity header is invalid: {:?}", source);
                    }
                }
            }

            trace!("l5d-client-id not enabled for {:?}", source);
            Ok(svc::Either::B(svc))
        }
    }

    // === impl Service ===

    impl<S: Clone, B> Clone for Service<S, B> {
        fn clone(&self) -> Self {
            Service {
                inner: self.inner.clone(),
                value: self.value.clone(),
                _marker: PhantomData,
            }
        }
    }

    impl<S, B> svc::Service<http::Request<B>> for Service<S, B>
    where
        S: svc::Service<http::Request<B>>,
    {
        type Response = S::Response;
        type Error = S::Error;
        type Future = S::Future;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.inner.poll_ready()
        }

        fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
            req.headers_mut()
                .insert(super::super::L5D_CLIENT_ID, self.value.clone());

            self.inner.call(req)
        }
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

    const TLS_DISABLED: tls::ConditionalIdentity = Conditional::None(tls::ReasonForNoTls::Disabled);

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
