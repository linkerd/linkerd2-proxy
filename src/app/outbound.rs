use indexmap::IndexMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::{fmt, hash};

use super::identity;
use control::destination::{Metadata, ProtocolHint};
use proxy::{
    self,
    http::{identity_from_header, settings},
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
    pub http_settings: settings::Settings,
}

// === impl Endpoint ===

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
        let addr = req
            .extensions()
            .get::<proxy::Source>()?
            .orig_dst_if_not_local()?;
        let http_settings = settings::Settings::from_request(req);
        let identity = match identity_from_header(req, super::L5D_REQUIRE_ID) {
            Some(require_id) => Conditional::Some(require_id),
            None => {
                Conditional::None(tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery.into())
            }
        };

        Some(Self {
            addr,
            dst_name: None,
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
            dst_name: None,
            identity: Conditional::None(tls::ReasonForNoPeerName::NotHttp.into()),
            metadata: Metadata::empty(),
            http_settings: settings::Settings::NotHttp,
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
    use futures::{future::Future, Async, Poll};
    use std::net::SocketAddr;

    use super::super::dst::DstAddr;
    use super::Endpoint;
    use control::destination::Metadata;
    use proxy::{http::settings, resolve};
    use transport::tls;
    use {Addr, Conditional, NameAddr};

    #[derive(Clone, Debug)]
    pub struct Resolve<R: resolve::Resolve<NameAddr>>(R);

    #[derive(Debug)]
    pub struct Resolution<R> {
        resolving: Resolving<R>,
        http_settings: settings::Settings,
    }

    #[derive(Debug)]
    enum Resolving<R> {
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
        type Future = Resolution<R::Future>;

        fn resolve(&self, dst: &DstAddr) -> Self::Future {
            let resolving = match dst.as_ref() {
                Addr::Name(ref name) => Resolving::Name(name.clone(), self.0.resolve(&name)),
                Addr::Socket(ref addr) => Resolving::Addr(Some(*addr)),
            };

            Resolution {
                http_settings: dst.http_settings,
                resolving,
            }
        }
    }

    // === impl Resolution ===

    impl<F: Future> Future for Resolution<F> {
        type Item = Resolution<F::Item>;
        type Error = F::Error;
        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            let resolving = match self.resolving {
                Resolving::Name(ref name, ref mut f) => {
                    let res = try_ready!(f.poll());
                    // TODO: get rid of unnecessary arc bumps?
                    Resolving::Name(name.clone(), res)
                }
                Resolving::Addr(a) => Resolving::Addr(a),
            };
            Ok(Async::Ready(Resolution {
                resolving,
                // TODO: get rid of unnecessary clone
                http_settings: self.http_settings.clone(),
            }))
        }
    }

    impl<R> resolve::Resolution for Resolution<R>
    where
        R: resolve::Resolution<Endpoint = Metadata>,
    {
        type Endpoint = Endpoint;
        type Error = R::Error;

        fn poll(&mut self) -> Poll<resolve::Update<Self::Endpoint>, Self::Error> {
            match self.resolving {
                Resolving::Name(ref name, ref mut res) => match try_ready!(res.poll()) {
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
                            http_settings: self.http_settings,
                        };
                        Ok(Async::Ready(resolve::Update::Add(addr, ep)))
                    }
                },
                Resolving::Addr(ref mut addr) => match addr.take() {
                    Some(addr) => {
                        let ep = Endpoint {
                            dst_name: None,
                            addr,
                            identity: Conditional::None(
                                tls::ReasonForNoPeerName::NoAuthorityInHttpRequest.into(),
                            ),
                            metadata: Metadata::empty(),
                            http_settings: self.http_settings,
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
    use proxy::http::{orig_proto, settings::Settings};
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

        fn call(&mut self, mut endpoint: Endpoint) -> Self::Future {
            let can_upgrade = endpoint.can_use_orig_proto();

            if can_upgrade {
                trace!(
                    "supporting {} upgrades for endpoint={:?}",
                    orig_proto::L5D_ORIG_PROTO,
                    endpoint,
                );
                endpoint.http_settings = Settings::Http2;
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

pub mod require_identity_on_endpoint {
    use super::Endpoint;
    use app::L5D_REQUIRE_ID;
    use futures::{
        future::{self, Either, FutureResult},
        Async, Future, Poll,
    };
    use identity;
    use proxy::http::{identity_from_header, HasH2Reason};
    use std::marker::PhantomData;
    use svc;
    use transport::tls::{self, HasPeerIdentity};
    use Conditional;

    type Error = Box<dyn std::error::Error + Send + Sync>;

    #[derive(Debug)]
    pub struct RequireIdentityError {
        require_identity: identity::Name,
        peer_identity: Option<identity::Name>,
    }

    pub struct Layer<A, B>(PhantomData<fn(A) -> B>);

    pub struct MakeSvc<M, A, B> {
        inner: M,
        _marker: PhantomData<fn(A) -> B>,
    }

    pub struct MakeFuture<F, A, B> {
        peer_identity: tls::PeerIdentity,
        inner: F,
        _marker: PhantomData<fn(A) -> B>,
    }

    pub struct RequireIdentity<M, A, B> {
        peer_identity: tls::PeerIdentity,
        inner: M,
        _marker: PhantomData<fn(A) -> B>,
    }

    // ===== impl Layer =====

    pub fn layer<A, B>() -> Layer<A, B> {
        Layer(PhantomData)
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

    impl<A, B> Clone for Layer<A, B> {
        fn clone(&self) -> Self {
            Layer(PhantomData)
        }
    }

    // ===== impl MakeSvc =====

    impl<M, A, B> svc::Service<Endpoint> for MakeSvc<M, A, B>
    where
        M: svc::MakeService<Endpoint, http::Request<A>, Response = http::Response<B>>,
    {
        type Response = RequireIdentity<M::Service, A, B>;
        type Error = M::MakeError;
        type Future = MakeFuture<M::Future, A, B>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.inner.poll_ready()
        }

        fn call(&mut self, target: Endpoint) -> Self::Future {
            // After the inner service is made, we want to wrap that service
            // with a filter that compares the target's `peer_identity` and
            // `l5d_require_id` header if present

            // After the inner service is made, we want to wrap that service
            // with a service that checks for the presence of the
            // `l5d-require-id` header. If is present then assert it is the
            // endpoint identity; otherwise fail the request.
            let peer_identity = target.peer_identity().clone();
            let inner = self.inner.make_service(target);

            MakeFuture {
                peer_identity,
                inner,
                _marker: PhantomData,
            }
        }
    }

    impl<F, A, B> Future for MakeFuture<F, A, B>
    where
        F: Future,
        F::Item: svc::Service<http::Request<A>, Response = http::Response<B>>,
    {
        type Item = RequireIdentity<F::Item, A, B>;
        type Error = F::Error;

        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            let inner = try_ready!(self.inner.poll());

            // The inner service is ready and we now create a new service
            // that filters based off `peer_identity` and `l5d-require-id`
            // header
            let svc = RequireIdentity {
                peer_identity: self.peer_identity.clone(),
                inner,
                _marker: PhantomData,
            };

            Ok(Async::Ready(svc))
        }
    }

    impl<M: Clone, A, B> Clone for MakeSvc<M, A, B> {
        fn clone(&self) -> Self {
            MakeSvc {
                inner: self.inner.clone(),
                _marker: PhantomData,
            }
        }
    }

    // ===== impl RequireIdentity =====

    impl<M, A, B> svc::Service<http::Request<A>> for RequireIdentity<M, A, B>
    where
        M: svc::Service<http::Request<A>, Response = http::Response<B>>,
        M::Error: Into<Error>,
    {
        type Response = M::Response;
        type Error = Error;
        type Future = Either<
            FutureResult<Self::Response, Self::Error>,
            future::MapErr<M::Future, fn(M::Error) -> Error>,
        >;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.inner.poll_ready().map_err(Into::into)
        }

        fn call(&mut self, request: http::Request<A>) -> Self::Future {
            // If the `l5d-require-id` header is present, then we should expect
            // the target's `peer_identity` to match; if the two values do not
            // match or there is no `peer_identity`, then we fail the request
            if let Some(require_identity) = identity_from_header(&request, L5D_REQUIRE_ID) {
                debug!("found l5d-require-id={:?}", require_identity.as_ref());
                match self.peer_identity {
                    Conditional::Some(ref peer_identity) => {
                        if require_identity != *peer_identity {
                            warn!(
                                "require identity check failed; found peer_identity={:?}",
                                peer_identity
                            );
                            return Either::A(future::err(RequireIdentityError::new(
                                require_identity,
                                Some(peer_identity.clone()),
                            )));
                        }
                    }
                    Conditional::None(_) => {
                        warn!("require identity check failed; no peer_identity found");
                        return Either::A(future::err(RequireIdentityError::new(
                            require_identity,
                            None,
                        )));
                    }
                }
            }

            Either::B(self.inner.call(request).map_err(Into::into))
        }
    }

    // ===== impl RequireIdentityError =====

    impl RequireIdentityError {
        fn new(require_identity: identity::Name, peer_identity: Option<identity::Name>) -> Error {
            let error = RequireIdentityError {
                require_identity,
                peer_identity,
            };

            error.into()
        }
    }

    impl std::error::Error for RequireIdentityError {}

    impl std::fmt::Display for RequireIdentityError {
        fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(
                f,
                "require identity check failed; require={:?} found={:?}",
                self.require_identity, self.peer_identity
            )
        }
    }

    impl HasH2Reason for RequireIdentityError {
        fn h2_reason(&self) -> Option<h2::Reason> {
            (self as &(dyn std::error::Error + 'static)).h2_reason()
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
