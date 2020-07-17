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
    use linkerd2_addr::Addr;
    use linkerd2_dns as dns;
    use linkerd2_error::Error;
    use linkerd2_proxy_core::resolve::{self, Update};
    use std::collections::{HashSet, VecDeque};
    use std::net::SocketAddr;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::time;
    use tracing::debug;

    #[derive(Clone)]
    pub struct Resolve {
        dns: dns::Resolver,
    }

    // === impl ControlPlaneResolve ===

    impl Resolve {
        pub fn new(dns: dns::Resolver) -> Self {
            Self { dns }
        }
    }

    type UpdatesStream =
        Pin<Box<dyn Stream<Item = Result<Update<Target>, Error>> + Send + 'static>>;

    impl tower::Service<ControlAddr> for Resolve {
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

    use async_stream::try_stream;
    use futures::prelude::*;
    use tokio::time::Instant;

    pub fn diff_targets(
        old_targets: &HashSet<Target>,
        new_targets: &HashSet<Target>,
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

    pub fn resolution_stream(
        dns: dns::Resolver,
        address: ControlAddr,
    ) -> impl Stream<Item = Result<Update<Target>, Error>> {
        return match address.clone().addr {
            Addr::Socket(_) => unimplemented!(),
            Addr::Name(name_addr) => {
                let port = address.addr.port().clone();
                let server_name = address.identity.clone();
                try_stream! {
                    let mut resolve_ips = Some(dns.resolve_ips(name_addr.name()));
                    let mut pending_updates: VecDeque<Update<Target>> = VecDeque::default();
                    let mut current_targets: HashSet<Target> = HashSet::new();
                    let mut ttl_expiration = time::delay_until(Instant::now());

                    if let Some(resolve_result) = resolve_ips {
                        debug_assert!(current_targets.is_empty());
                        debug_assert!(pending_updates.is_empty());

                        match resolve_result.await {
                            Err(dns::Error::NoAddressesFound(valid_until)) => {
                                resolve_ips = None;
                                debug!("resolved empty");
                                current_targets.clear();
                                ttl_expiration = time::delay_until(valid_until);
                            }

                            Err(dns::Error::ResolutionFailed(inner)) => {
                                resolve_ips = None;
                                if let dns::ResolveErrorKind::NoRecordsFound {
                                    valid_until: Some(valid_until),
                                    ..
                                } = inner.kind()
                                {
                                    debug!("resolved does not exist");
                                    current_targets.clear();
                                    pending_updates.push_back(Update::DoesNotExist);
                                    ttl_expiration = time::delay_until(time::Instant::from_std(
                                        valid_until.clone(),
                                    ));
                                } else {
                                    Err(dns::Error::ResolutionFailed(inner))?
                                }
                            }
                            Ok(result) => {
                                resolve_ips = None;
                                let new_targets = result
                                    .ips
                                    .into_iter()
                                    .map(|ip| Target {
                                        addr: SocketAddr::from((ip, port)),
                                        server_name: server_name.clone(),
                                    })
                                    .collect();

                                let (adds, removes) = diff_targets(&current_targets, &new_targets);
                                if !adds.is_empty() {
                                    pending_updates.push_back(Update::Add(adds))
                                }
                                if !removes.is_empty() {
                                    pending_updates.push_back(Update::Remove(removes))
                                }

                                current_targets.clear();
                                current_targets.extend(new_targets);

                                debug!(?pending_updates, "resolved");
                                ttl_expiration = time::delay_until(result.valid_until);
                            }
                            Err(err) => {
                                resolve_ips = None;
                                Err(err)?
                            }
                        };
                    }

                    while let Some(update) = pending_updates.pop_front() {
                        yield update;
                    }

                    ttl_expiration.await;
                    resolve_ips = Some(dns.resolve_ips(name_addr.name()));
                }
            }
        };
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
