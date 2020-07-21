use linkerd2_addr::Addr;
use std::fmt;

#[derive(Clone, Debug)]
pub struct ControlAddr {
    pub addr: Addr,
    pub identity: crate::transport::tls::PeerIdentity,
}

impl fmt::Display for ControlAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.addr, f)
    }
}

/// Sets the request's URI from `Config`.
pub mod add_origin {
    use super::ControlAddr;
    use futures::{ready, TryFuture};
    use linkerd2_error::Error;
    use pin_project::pin_project;
    use std::future::Future;
    use std::marker::PhantomData;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tower_request_modifier::{Builder, RequestModifier};

    #[derive(Debug)]
    pub struct Layer<B> {
        _marker: PhantomData<fn(B)>,
    }

    #[derive(Debug)]
    pub struct MakeAddOrigin<M, B> {
        inner: M,
        _marker: PhantomData<fn(B)>,
    }

    #[pin_project]
    pub struct MakeFuture<F, B> {
        #[pin]
        inner: F,
        authority: http::uri::Authority,
        _marker: PhantomData<fn(B)>,
    }

    // === impl Layer ===

    impl<B> Layer<B> {
        pub fn new() -> Self {
            Layer {
                _marker: PhantomData,
            }
        }
    }

    impl<B> Clone for Layer<B> {
        fn clone(&self) -> Self {
            Self {
                _marker: self._marker,
            }
        }
    }

    impl<M, B> tower::layer::Layer<M> for Layer<B> {
        type Service = MakeAddOrigin<M, B>;

        fn layer(&self, inner: M) -> Self::Service {
            Self::Service {
                inner,
                _marker: PhantomData,
            }
        }
    }

    // === impl MakeAddOrigin ===

    impl<M, B> tower::Service<ControlAddr> for MakeAddOrigin<M, B>
    where
        M: tower::Service<ControlAddr>,
        M::Error: Into<Error>,
    {
        type Response = RequestModifier<M::Response, B>;
        type Error = Error;
        type Future = MakeFuture<M::Future, B>;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.inner.poll_ready(cx).map_err(Into::into)
        }

        fn call(&mut self, target: ControlAddr) -> Self::Future {
            let authority = target.addr.to_http_authority();
            let inner = self.inner.call(target);
            MakeFuture {
                inner,
                authority,
                _marker: PhantomData,
            }
        }
    }

    impl<M, B> Clone for MakeAddOrigin<M, B>
    where
        M: tower::Service<ControlAddr> + Clone,
    {
        fn clone(&self) -> Self {
            Self {
                inner: self.inner.clone(),
                _marker: PhantomData,
            }
        }
    }

    // === impl MakeFuture ===

    impl<F, B> Future for MakeFuture<F, B>
    where
        F: TryFuture,
        F::Error: Into<Error>,
    {
        type Output = Result<RequestModifier<F::Ok, B>, Error>;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.project();
            let inner = ready!(this.inner.try_poll(cx).map_err(Into::into))?;

            Poll::Ready(
                Builder::new()
                    .set_origin(format!("http://{}", this.authority))
                    .build(inner)
                    .map_err(|_| BuildError.into()),
            )
        }
    }

    // XXX the request_modifier build error does not implement Error...
    #[derive(Debug)]
    struct BuildError;

    impl std::error::Error for BuildError {}
    impl std::fmt::Display for BuildError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "failed to build the add-origin request modifier")
        }
    }
}

pub mod dns_resolve {
    use super::{client::Target, ControlAddr};
    use async_stream::try_stream;
    use futures::future;
    use futures::prelude::*;
    use futures::stream::StreamExt;
    use linkerd2_addr::Addr;
    use linkerd2_dns as dns;
    use linkerd2_error::Error;
    use linkerd2_proxy_core::resolve::{self, Update};
    use std::net::SocketAddr;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::time;
    use tracing::debug;

    #[derive(Clone)]
    pub struct Resolve<R> {
        dns: R,
    }

    // === impl ControlPlaneResolve ===

    impl<R> Resolve<R> {
        pub fn new(dns: R) -> Self {
            Self { dns }
        }
    }

    type UpdatesStream =
        Pin<Box<dyn Stream<Item = Result<Update<Target>, Error>> + Send + 'static>>;

    impl<R> tower::Service<ControlAddr> for Resolve<R>
    where
        R: dns::Resolver + Send + Sync + 'static,
    {
        type Response = resolve::FromStream<UpdatesStream>;
        type Error = Error;
        type Future = futures::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, target: ControlAddr) -> Self::Future {
            futures::future::ok(resolve::from_stream::<UpdatesStream>(Box::pin(
                resolution_stream(self.dns.clone(), target.clone()),
            )))
        }
    }

    // === impl ControlPlaneResolution ===

    pub fn diff_targets(
        old_targets: &Vec<Target>,
        new_targets: &Vec<Target>,
    ) -> (Vec<(SocketAddr, Target)>, Vec<SocketAddr>) {
        let mut adds = Vec::default();
        let mut removes = Vec::default();

        for new_target in new_targets.iter() {
            if !old_targets.contains(new_target) {
                adds.push((new_target.addr, new_target.clone()))
            }
        }

        for old_target in old_targets.iter() {
            if !new_targets.contains(old_target) {
                removes.push(old_target.addr)
            }
        }

        (adds, removes)
    }

    pub fn resolution_stream<R>(
        dns: R,
        address: ControlAddr,
    ) -> impl Stream<Item = Result<Update<Target>, Error>>
    where
        R: dns::Resolver,
    {
        let port = address.addr.port().clone();
        let server_name = address.identity.clone();
        try_stream! {
            match address.clone().addr {
                Addr::Socket(sa) => {
                    // this should yield the socket address once
                    // and keep on yielding NotReady forever.
                    let mut stream = futures::stream::once(future::ok::<Update<Target>, Error>(
                        Update::Add(vec![(sa, Target::new(sa, address.identity))]),
                    ))
                    .chain(tokio::stream::pending());

                    while let Some(value) = stream.next().await {
                        yield value.expect("no errors in this stream");
                    }
                }
                Addr::Name(name_addr) => {
                    let mut current_targets: Vec<Target> = Vec::new();
                    loop {
                        let mut resolve_ips = Some(dns.resolve_ips(name_addr.name()));
                        if let Some(resolve_result) = resolve_ips {
                            match resolve_result.await {
                                Err(dns::Error::NoAddressesFound(valid_until, exists)) => {
                                    resolve_ips = None;
                                    debug!("resolved empty");
                                    current_targets.clear();
                                    if exists {
                                        yield Update::Empty;
                                    } else {
                                        yield Update::DoesNotExist;
                                    }
                                    time::delay_until(valid_until).await;
                                }
                                Ok(result) => {
                                    resolve_ips = None;
                                    let new_targets = result
                                        .ips
                                        .into_iter()
                                        .map(|ip| {
                                            Target::new(
                                                SocketAddr::from((ip, port)),
                                                server_name.clone(),
                                            )
                                        })
                                        .collect();

                                    let (adds, removes) =
                                        diff_targets(&current_targets, &new_targets);

                                    if !adds.is_empty() {
                                        yield Update::Add(adds);
                                    }

                                    if !removes.is_empty() {
                                        yield Update::Remove(removes);
                                    }

                                    current_targets.clear();
                                    current_targets.extend(new_targets);

                                    time::delay_until(result.valid_until).await;
                                }
                                Err(err) => {
                                    Err(err)?;
                                }
                            };
                        }
                    }
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use async_trait::async_trait;
        use linkerd2_proxy_identity::Name;
        use linkerd2_proxy_transport::tls::Conditional;
        use linkerd2_proxy_transport::tls::PeerIdentity;
        use std::collections::VecDeque;
        use std::sync::Arc;
        use std::sync::RwLock;
        use tokio::time::{self, Duration, Instant};
        use tokio_test::{assert_pending, assert_ready, assert_ready_eq, task};

        #[derive(Clone)]
        struct MockDnsResolver {
            responses: Arc<RwLock<VecDeque<Result<dns::ResolveResponse, dns::Error>>>>,
        }

        impl MockDnsResolver {
            pub fn new(responses: Vec<Result<dns::ResolveResponse, dns::Error>>) -> Self {
                MockDnsResolver {
                    responses: Arc::new(RwLock::new(responses.into_iter().collect())),
                }
            }
        }

        #[async_trait]
        impl dns::Resolver for MockDnsResolver {
            async fn lookup_ip(
                &self,
                _name: dns::Name,
                _span: tracing::Span,
            ) -> Result<dns::LookupIp, dns::Error> {
                unimplemented!()
            }
            async fn resolve_ips(
                &self,
                _name: &dns::Name,
            ) -> Result<dns::ResolveResponse, dns::Error> {
                self.responses.write().unwrap().pop_front().unwrap()
            }
            fn into_make_refine(self) -> dns::MakeRefine<Self> {
                unimplemented!()
            }
        }

        fn add(adresses: Vec<&str>) -> resolve::Update<Target> {
            resolve::Update::Add(
                adresses
                    .into_iter()
                    .map(|a| a.parse().unwrap())
                    .map(|addr| (addr, Target::new(addr, identity())))
                    .collect(),
            )
        }

        fn remove(adresses: Vec<&str>) -> resolve::Update<Target> {
            resolve::Update::Remove(adresses.into_iter().map(|a| a.parse().unwrap()).collect())
        }

        fn dns_rsp(addresses: Vec<&str>, ttl: Duration) -> dns::ResolveResponse {
            dns::ResolveResponse {
                ips: addresses.into_iter().map(|a| a.parse().unwrap()).collect(),
                valid_until: Instant::now() + ttl,
            }
        }

        fn identity() -> PeerIdentity {
            Conditional::Some(Name::from_hostname("test.identity".as_bytes()).unwrap())
        }

        fn address(addr: &str) -> ControlAddr {
            ControlAddr {
                addr: Addr::from_str(addr).unwrap(),
                identity: identity(),
            }
        }

        #[tokio::test]
        async fn yields_add() {
            let dns_resolver = MockDnsResolver::new(vec![Ok(dns_rsp(
                vec!["127.0.0.1"],
                Duration::from_millis(100),
            ))]);

            let mut stream = task::spawn(
                resolution_stream(dns_resolver, address("test.com:8888"))
                    .map_err(|err| err.to_string()),
            );

            assert_ready_eq!(stream.poll_next(), Some(Ok(add(vec!["127.0.0.1:8888"]))));
            assert_pending!(stream.poll_next());
        }

        #[tokio::test]
        async fn yields_remove_on_different_rsp() {
            time::pause();
            let dns_resolver = MockDnsResolver::new(vec![
                Ok(dns_rsp(vec!["127.0.0.1"], Duration::from_millis(100))),
                Ok(dns_rsp(vec!["127.0.0.2"], Duration::from_millis(200))),
            ]);

            let mut stream = task::spawn(
                resolution_stream(dns_resolver, address("test.com:8888"))
                    .map_err(|err| err.to_string()),
            );

            assert_ready_eq!(stream.poll_next(), Some(Ok(add(vec!["127.0.0.1:8888"]))));
            time::advance(Duration::from_millis(101)).await;
            assert_ready_eq!(stream.poll_next(), Some(Ok(add(vec!["127.0.0.2:8888"]))));
            assert_ready_eq!(stream.poll_next(), Some(Ok(remove(vec!["127.0.0.1:8888"]))));
            // should return pending because it is awaiting on the dns TTL
            assert_pending!(stream.poll_next());
        }

        #[tokio::test]
        async fn yields_does_not_exist() {
            time::pause();
            let dns_resolver = MockDnsResolver::new(vec![
                Ok(dns_rsp(
                    vec!["127.0.0.1", "127.0.0.2", "127.0.0.3", "127.0.0.4"],
                    Duration::from_millis(100),
                )),
                Err(dns::Error::NoAddressesFound(
                    Instant::now() + Duration::from_millis(200),
                    false,
                )),
                Ok(dns_rsp(vec!["127.0.0.1"], Duration::from_millis(300))),
            ]);

            let mut stream = task::spawn(
                resolution_stream(dns_resolver, address("test.com:8888"))
                    .map_err(|err| err.to_string()),
            );
            assert_ready_eq!(
                stream.poll_next(),
                Some(Ok(add(vec![
                    "127.0.0.1:8888",
                    "127.0.0.2:8888",
                    "127.0.0.3:8888",
                    "127.0.0.4:8888"
                ])))
            );
            time::advance(Duration::from_millis(101)).await;
            assert_ready_eq!(stream.poll_next(), Some(Ok(Update::DoesNotExist)));
            time::advance(Duration::from_millis(101)).await;
            assert_ready_eq!(stream.poll_next(), Some(Ok(add(vec!["127.0.0.1:8888"]))));
        }

        #[tokio::test]
        async fn yields_empty() {
            time::pause();
            let dns_resolver = MockDnsResolver::new(vec![
                Ok(dns_rsp(
                    vec!["127.0.0.1", "127.0.0.2", "127.0.0.3", "127.0.0.4"],
                    Duration::from_millis(100),
                )),
                Err(dns::Error::NoAddressesFound(
                    Instant::now() + Duration::from_millis(200),
                    true,
                )),
                Ok(dns_rsp(vec!["127.0.0.1"], Duration::from_millis(300))),
            ]);

            let mut stream = task::spawn(
                resolution_stream(dns_resolver, address("test.com:8888"))
                    .map_err(|err| err.to_string()),
            );
            assert_ready_eq!(
                stream.poll_next(),
                Some(Ok(add(vec![
                    "127.0.0.1:8888",
                    "127.0.0.2:8888",
                    "127.0.0.3:8888",
                    "127.0.0.4:8888"
                ])))
            );
            time::advance(Duration::from_millis(101)).await;
            assert_ready_eq!(stream.poll_next(), Some(Ok(Update::Empty)));
            time::advance(Duration::from_millis(101)).await;
            assert_ready_eq!(stream.poll_next(), Some(Ok(add(vec!["127.0.0.1:8888"]))));
        }

        #[tokio::test]
        #[ignore]
        async fn continues_yielding_after_err() {
            time::pause();
            let dns_resolver = MockDnsResolver::new(vec![
                Err(dns::Error::TaskLost),
                Ok(dns_rsp(vec!["127.0.0.1"], Duration::from_millis(100))),
            ]);

            let mut stream = task::spawn(
                resolution_stream(dns_resolver, address("test.com:8888"))
                    .map_err(|err| err.to_string()),
            );
            let result = assert_ready!(stream.poll_next());
            assert!(result.unwrap().is_err());
            assert_ready_eq!(stream.poll_next(), Some(Ok(add(vec!["127.0.0.1:8888"]))));
        }

        #[tokio::test]
        async fn yields_once_for_ip_addr() {
            time::pause();
            let dns_resolver = MockDnsResolver::new(vec![]);
            let mut stream = task::spawn(
                resolution_stream(dns_resolver, address("127.0.0.5:8888"))
                    .map_err(|err| err.to_string()),
            );
            assert_ready_eq!(stream.poll_next(), Some(Ok(add(vec!["127.0.0.5:8888"]))));
            time::advance(Duration::from_millis(1000)).await;
            assert_pending!(stream.poll_next());
        }
    }
}

/// Creates a client suitable for gRPC.
pub mod client {
    use crate::transport::{connect, tls};
    use crate::{proxy::http, svc};
    use linkerd2_proxy_http::h2::Settings as H2Settings;
    use std::net::SocketAddr;
    use std::task::{Context, Poll};

    #[derive(Clone, Hash, Debug, Eq, PartialEq)]
    pub struct Target {
        pub(super) addr: SocketAddr,
        pub(super) server_name: tls::PeerIdentity,
    }

    impl Target {
        pub fn new(addr: SocketAddr, server_name: tls::PeerIdentity) -> Self {
            Self { addr, server_name }
        }
    }

    #[derive(Debug)]
    pub struct Client<C, B> {
        inner: http::h2::Connect<C, B>,
    }

    // === impl Target ===

    impl connect::ConnectAddr for Target {
        fn connect_addr(&self) -> SocketAddr {
            self.addr
        }
    }

    impl tls::HasPeerIdentity for Target {
        fn peer_identity(&self) -> tls::PeerIdentity {
            self.server_name.clone()
        }
    }

    // === impl Layer ===

    pub fn layer<C, B>() -> impl svc::Layer<C, Service = Client<C, B>> + Copy
    where
        http::h2::Connect<C, B>: tower::Service<Target>,
    {
        svc::layer::mk(|mk_conn| {
            let inner = http::h2::Connect::new(mk_conn, H2Settings::default());
            Client { inner }
        })
    }

    // === impl Client ===

    impl<C, B> tower::Service<Target> for Client<C, B>
    where
        http::h2::Connect<C, B>: tower::Service<Target>,
    {
        type Response = <http::h2::Connect<C, B> as tower::Service<Target>>::Response;
        type Error = <http::h2::Connect<C, B> as tower::Service<Target>>::Error;
        type Future = <http::h2::Connect<C, B> as tower::Service<Target>>::Future;

        #[inline]
        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.inner.poll_ready(cx)
        }

        #[inline]
        fn call(&mut self, target: Target) -> Self::Future {
            self.inner.call(target)
        }
    }

    // A manual impl is needed since derive adds `B: Clone`, but that's just
    // a PhantomData.
    impl<C, B> Clone for Client<C, B>
    where
        http::h2::Connect<C, B>: Clone,
    {
        fn clone(&self) -> Self {
            Client {
                inner: self.inner.clone(),
            }
        }
    }
}
