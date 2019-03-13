use indexmap::IndexMap;
use std::sync::Arc;
use std::{fmt, net};

use super::identity;
use control::destination::{Metadata, ProtocolHint};
use proxy::http::settings;
use svc;
use tap;
use transport::{connect, tls};
use {Conditional, NameAddr};

#[derive(Clone, Debug)]
pub struct Endpoint {
    pub dst_name: Option<NameAddr>,
    pub addr: net::SocketAddr,
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
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.addr.fmt(f)
    }
}

impl tap::Inspect for Endpoint {
    fn src_addr<B>(&self, req: &http::Request<B>) -> Option<net::SocketAddr> {
        use proxy::server::Source;

        req.extensions().get::<Source>().map(|s| s.remote)
    }

    fn src_tls<'a, B>(
        &self,
        _: &'a http::Request<B>,
    ) -> Conditional<&'a identity::Name, tls::ReasonForNoIdentity> {
        Conditional::None(tls::ReasonForNoPeerName::Loopback.into())
    }

    fn dst_addr<B>(&self, _: &http::Request<B>) -> Option<net::SocketAddr> {
        Some(self.addr)
    }

    fn dst_labels<B>(&self, _: &http::Request<B>) -> Option<&IndexMap<String, String>> {
        Some(self.metadata.labels())
    }

    fn dst_tls<B>(
        &self,
        _: &http::Request<B>,
    ) -> Conditional<&identity::Name, tls::ReasonForNoIdentity> {
        self.metadata.tls_server_identity()
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
    use transport::{connect, tls};
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
                    resolve::Update::Remove(addr) => {
                        debug!("removing {}", addr);
                        Ok(Async::Ready(resolve::Update::Remove(addr)))
                    }
                    resolve::Update::Add(addr, metadata) => {
                        debug!("adding {}", addr);
                        let ep = Endpoint {
                            dst_name: Some(name.clone()),
                            addr,
                            metadata,
                        };
                        Ok(Async::Ready(resolve::Update::Add(addr, ep)))
                    }
                },
                Resolution::Addr(ref mut addr) => match addr.take() {
                    Some(addr) => {
                        let tls = tls::ReasonForNoPeerName::NoAuthorityInHttpRequest;
                        let ep = Endpoint {
                            dst_name: None,
                            connect: connect::Target::new(addr, Conditional::None(tls.into())),
                            metadata: Metadata::none(tls),
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

    use http;

    use super::Endpoint;
    use proxy::http::orig_proto;
    use svc;

    #[derive(Debug)]
    pub struct Layer<A, B>(PhantomData<fn(A) -> B>);

    #[derive(Debug)]
    pub struct Stack<M, A, B> {
        inner: M,
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

    impl<M, A, B> svc::Layer<Endpoint, Endpoint, M> for Layer<A, B>
    where
        M: svc::Stack<Endpoint>,
        M::Value: svc::Service<http::Request<A>, Response = http::Response<B>>,
    {
        type Value = <Stack<M, A, B> as svc::Stack<Endpoint>>::Value;
        type Error = <Stack<M, A, B> as svc::Stack<Endpoint>>::Error;
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

    impl<M, A, B> svc::Stack<Endpoint> for Stack<M, A, B>
    where
        M: svc::Stack<Endpoint>,
        M::Value: svc::Service<http::Request<A>, Response = http::Response<B>>,
    {
        type Value = svc::Either<orig_proto::Upgrade<M::Value>, M::Value>;
        type Error = M::Error;

        fn make(&self, endpoint: &Endpoint) -> Result<Self::Value, Self::Error> {
            if endpoint.can_use_orig_proto() {
                trace!(
                    "supporting {} upgrades for endpoint={:?}",
                    orig_proto::L5D_ORIG_PROTO,
                    endpoint,
                );
                self.inner
                    .make(&endpoint)
                    .map(|i| svc::Either::A(orig_proto::Upgrade::new(i)))
            } else {
                self.inner.make(&endpoint).map(svc::Either::B)
            }
        }
    }
}

/// Adds `l5d-server-id` headers to http::Responses derived from the
/// TlsIdentity of an `Endpoint`.
pub mod add_server_id_on_rsp {
    use super::super::L5D_SERVER_ID;
    use super::Endpoint;
    use http::header::HeaderValue;
    use proxy::http::add_header::{self, response::ResHeader, Layer};
    use Conditional;

    pub fn layer() -> Layer<&'static str, Endpoint, ResHeader> {
        add_header::response::layer(L5D_SERVER_ID, |endpoint: &Endpoint| {
            if let Conditional::Some(id) = endpoint.connect.tls_server_identity() {
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
pub mod add_remote_ip_on_rsp {
    use super::super::L5D_REMOTE_IP;
    use super::Endpoint;
    use bytes::Bytes;
    use http::header::HeaderValue;
    use proxy::http::add_header::{self, response::ResHeader, Layer};

    pub fn layer() -> Layer<&'static str, Endpoint, ResHeader> {
        add_header::response::layer(L5D_REMOTE_IP, |endpoint: &Endpoint| {
            HeaderValue::from_shared(Bytes::from(endpoint.connect.addr.ip().to_string())).ok()
        })
    }
}
