use indexmap::IndexMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::{fmt, hash};

use super::identity;
use control::destination::{Metadata, ProtocolHint};
use proxy::{
    self,
    http::balance::{HasWeight, Weight},
};
use tap;
use transport::{connect, tls};
use {Conditional, NameAddr};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Endpoint {
    pub dst_name: Option<NameAddr>,
    pub addr: SocketAddr,
    pub identity: tls::PeerIdentity,
    pub metadata: Metadata,
}

// === impl Endpoint ===

impl Endpoint {
    pub fn can_use_orig_proto(&self) -> bool {
        match self.metadata.protocol_hint() {
            ProtocolHint::Unknown => false,
            ProtocolHint::Http2 => true,
        }
    }

    pub fn from_orig_dst(source: &proxy::Source) -> Option<Self> {
        let addr = source.orig_dst_if_not_local()?;
        Some(Self {
            addr,
            dst_name: None,
            identity: Conditional::None(
                tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery.into(),
            ),
            metadata: Metadata::empty(),
        })
    }
}

impl From<SocketAddr> for Endpoint {
    fn from(addr: SocketAddr) -> Self {
        Self {
            addr,
            dst_name: None,
            identity: Conditional::None(tls::ReasonForNoPeerName::NotHttp.into()),
            metadata: Metadata::empty(),
        }
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.addr.fmt(f)
    }
}

impl hash::Hash for Endpoint {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.dst_name.hash(state);
        self.addr.hash(state);
        self.identity.hash(state);
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

impl HasWeight for Endpoint {
    fn weight(&self) -> Weight {
        self.metadata.weight()
    }
}

impl tap::Inspect for Endpoint {
    fn src_addr<B>(&self, req: &http::Request<B>) -> Option<SocketAddr> {
        use proxy::server::Source;

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
        req.extensions()
            .get::<super::dst::Route>()
            .map(|r| r.labels().clone())
    }

    fn is_outbound<B>(&self, _: &http::Request<B>) -> bool {
        true
    }
}

pub mod discovery {
    use futures::{Async, Poll};
    use std::net::SocketAddr;

    use super::super::dst::DstAddr;
    use super::Endpoint;
    use control::destination::Metadata;
    use proxy::resolve;
    use transport::tls;
    use {Addr, Conditional, NameAddr};

    #[derive(Clone, Debug)]
    pub struct Resolve<R: resolve::Resolve<NameAddr>>(R);

    #[derive(Debug)]
    pub enum Resolution<R: resolve::Resolution> {
        Name(NameAddr, R),
        Addr(Option<SocketAddr>),
    }

    // === impl Resolve ===

    impl<R> Resolve<R>
    where
        R: resolve::Resolve<NameAddr, Endpoint = Metadata>,
    {
        pub fn new(resolve: R) -> Self {
            Resolve(resolve)
        }
    }

    impl<R> resolve::Resolve<DstAddr> for Resolve<R>
    where
        R: resolve::Resolve<NameAddr, Endpoint = Metadata>,
    {
        type Endpoint = Endpoint;
        type Resolution = Resolution<R::Resolution>;

        fn resolve(&self, dst: &DstAddr) -> Self::Resolution {
            match dst.as_ref() {
                Addr::Name(ref name) => Resolution::Name(name.clone(), self.0.resolve(&name)),
                Addr::Socket(ref addr) => Resolution::Addr(Some(*addr)),
            }
        }
    }

    // === impl Resolution ===

    impl<R> resolve::Resolution for Resolution<R>
    where
        R: resolve::Resolution<Endpoint = Metadata>,
    {
        type Endpoint = Endpoint;
        type Error = R::Error;

        fn poll(&mut self) -> Poll<resolve::Update<Self::Endpoint>, Self::Error> {
            match self {
                Resolution::Name(ref name, ref mut res) => match try_ready!(res.poll()) {
                    resolve::Update::NoEndpoints => Ok(Async::Ready(resolve::Update::NoEndpoints)),
                    resolve::Update::Remove(addr) => {
                        debug!("removing {}", addr);
                        Ok(Async::Ready(resolve::Update::Remove(addr)))
                    }
                    resolve::Update::Add(addr, metadata) => {
                        let identity = metadata
                            .identity()
                            .cloned()
                            .map(Conditional::Some)
                            .unwrap_or_else(|| {
                                Conditional::None(
                                    tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery.into(),
                                )
                            });
                        debug!("adding addr={}; identity={:?}", addr, identity);
                        let ep = Endpoint {
                            dst_name: Some(name.clone()),
                            addr,
                            identity,
                            metadata,
                        };
                        Ok(Async::Ready(resolve::Update::Add(addr, ep)))
                    }
                },
                Resolution::Addr(ref mut addr) => match addr.take() {
                    Some(addr) => {
                        let ep = Endpoint {
                            dst_name: None,
                            addr,
                            identity: Conditional::None(
                                tls::ReasonForNoPeerName::NoAuthorityInHttpRequest.into(),
                            ),
                            metadata: Metadata::empty(),
                        };
                        Ok(Async::Ready(resolve::Update::Add(addr, ep)))
                    }
                    None => Ok(Async::NotReady),
                },
            }
        }
    }
}

pub mod orig_proto_upgrade {
    use std::marker::PhantomData;

    use futures::{Future, Poll};
    use http;

    use super::Endpoint;
    use proxy::http::orig_proto;
    use svc;

    #[derive(Debug)]
    pub struct Layer<A, B>(PhantomData<fn(A) -> B>);

    #[derive(Debug)]
    pub struct MakeSvc<M, A, B> {
        inner: M,
        _marker: PhantomData<fn(A) -> B>,
    }

    pub struct MakeFuture<F, A, B> {
        can_upgrade: bool,
        inner: F,
        _marker: PhantomData<fn(A) -> B>,
    }

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
        M: svc::MakeService<Endpoint, http::Request<A>, Response = http::Response<B>>,
    {
        type Service = MakeSvc<M, A, B>;

        fn layer(&self, inner: M) -> Self::Service {
            MakeSvc {
                inner,
                _marker: PhantomData,
            }
        }
    }

    // === impl MakeSvc ===

    impl<M: Clone, A, B> Clone for MakeSvc<M, A, B> {
        fn clone(&self) -> Self {
            MakeSvc {
                inner: self.inner.clone(),
                _marker: PhantomData,
            }
        }
    }

    impl<M, A, B> svc::Service<Endpoint> for MakeSvc<M, A, B>
    where
        M: svc::MakeService<Endpoint, http::Request<A>, Response = http::Response<B>>,
    {
        type Response = svc::Either<orig_proto::Upgrade<M::Service>, M::Service>;
        type Error = M::MakeError;
        type Future = MakeFuture<M::Future, A, B>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.inner.poll_ready()
        }

        fn call(&mut self, endpoint: Endpoint) -> Self::Future {
            let can_upgrade = endpoint.can_use_orig_proto();

            if can_upgrade {
                trace!(
                    "supporting {} upgrades for endpoint={:?}",
                    orig_proto::L5D_ORIG_PROTO,
                    endpoint,
                );
            }

            let inner = self.inner.make_service(endpoint);
            MakeFuture {
                can_upgrade,
                inner,
                _marker: PhantomData,
            }
        }
    }

    // === impl MakeFuture ===

    impl<F, A, B> Future for MakeFuture<F, A, B>
    where
        F: Future,
        F::Item: svc::Service<http::Request<A>, Response = http::Response<B>>,
    {
        type Item = svc::Either<orig_proto::Upgrade<F::Item>, F::Item>;
        type Error = F::Error;

        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            let inner = try_ready!(self.inner.poll());

            if self.can_upgrade {
                Ok(svc::Either::A(orig_proto::Upgrade::new(inner)).into())
            } else {
                Ok(svc::Either::B(inner).into())
            }
        }
    }
}

/// Adds `l5d-server-id` headers to http::Responses derived from the
/// TlsIdentity of an `Endpoint`.
#[allow(dead_code)] // TODO #2597
pub mod add_server_id_on_rsp {
    use super::super::L5D_SERVER_ID;
    use super::Endpoint;
    use http::header::HeaderValue;
    use proxy::http::add_header::{self, response::ResHeader, Layer};
    use Conditional;

    pub fn layer() -> Layer<&'static str, Endpoint, ResHeader> {
        add_header::response::layer(L5D_SERVER_ID, |endpoint: &Endpoint| {
            if let Conditional::Some(id) = endpoint.identity.as_ref() {
                match HeaderValue::from_str(id.as_ref()) {
                    Ok(value) => {
                        debug!("l5d-server-id enabled for {:?}", endpoint);
                        return Some(value);
                    }
                    Err(_err) => {
                        warn!("l5d-server-id identity header is invalid: {:?}", endpoint);
                    }
                };
            }

            None
        })
    }
}

/// Adds `l5d-remote-ip` headers to http::Responses derived from the
/// `remote` of a `Source`.
#[allow(dead_code)] // TODO #2597
pub mod add_remote_ip_on_rsp {
    use super::super::L5D_REMOTE_IP;
    use super::Endpoint;
    use bytes::Bytes;
    use http::header::HeaderValue;
    use proxy::http::add_header::{self, response::ResHeader, Layer};

    pub fn layer() -> Layer<&'static str, Endpoint, ResHeader> {
        add_header::response::layer(L5D_REMOTE_IP, |endpoint: &Endpoint| {
            HeaderValue::from_shared(Bytes::from(endpoint.addr.ip().to_string())).ok()
        })
    }
}
