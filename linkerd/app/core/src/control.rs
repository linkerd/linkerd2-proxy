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

    // === impl Resolve ===

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
    use futures::future::{self, Either};
    use futures::prelude::*;
    use linkerd2_addr::{Addr, NameAddr};
    use linkerd2_error::Error;
    use linkerd2_proxy_core::resolve::Update;
    use linkerd2_proxy_transport::tls::PeerIdentity;
    use pin_project::pin_project;
    use std::net::SocketAddr;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    type UpdatesStream = Pin<Box<dyn Stream<Item = Result<Update<Target>, Error>> + Send + Sync>>;

    #[derive(Clone)]
    pub struct Resolve<S> {
        inner: S,
    }

    #[pin_project]
    pub struct MakeFuture<S: tower::Service<NameAddr>> {
        #[pin]
        inner: S::Future,
        identity: PeerIdentity,
    }

    // === impl Resolve ===

    impl<S> Resolve<S> {
        pub fn new(inner: S) -> Self {
            Self { inner }
        }

        // should yield the socket address once
        // and keep on yielding Pending forever.
        pub fn resolve_once_stream(&self, sa: SocketAddr, identity: PeerIdentity) -> UpdatesStream {
            Box::pin(try_stream! {
                let update = Update::Add(vec![(sa, Target::new(sa, identity))]);
                tracing::debug!(?update, "resolved once");
                yield update;
                future::pending::<Update<Target>>().await;
            })
        }
    }

    impl<S> tower::Service<ControlAddr> for Resolve<S>
    where
        S: tower::Service<NameAddr>,
        S::Response: Stream<Item = Result<Update<SocketAddr>, Error>> + Send + Sync + 'static,
        S::Error: Into<Error> + Send + Sync,
        S::Future: Send + Sync,
    {
        type Response = UpdatesStream;
        type Error = S::Error;
        type Future = Either<MakeFuture<S>, future::Ready<Result<Self::Response, S::Error>>>;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.inner.poll_ready(cx)
        }

        fn call(&mut self, target: ControlAddr) -> Self::Future {
            match target.addr {
                Addr::Name(na) if na.is_localhost() => {
                    let local_sa = format!("127.0.0.1:{}", na.port()).parse().unwrap();
                    Either::Right(future::ok(
                        self.resolve_once_stream(local_sa, target.identity),
                    ))
                }
                Addr::Name(na) => Either::Left(MakeFuture {
                    inner: self.inner.call(na),
                    identity: target.identity,
                }),
                Addr::Socket(sa) => {
                    Either::Right(future::ok(self.resolve_once_stream(sa, target.identity)))
                }
            }
        }
    }

    // === impl MakeFuture ===

    impl<S> Future for MakeFuture<S>
    where
        S: tower::Service<NameAddr>,
        S::Response: Stream<Item = Result<Update<SocketAddr>, Error>> + Send + Sync + 'static,
        S::Error: Into<Error> + Send + Sync,
        S::Future: Send + Sync,
    {
        type Output = Result<UpdatesStream, S::Error>;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.project();
            let identity = this.identity.clone();
            let stream = futures::ready!(this.inner.poll(cx))?;
            Poll::Ready(Ok(Box::pin(stream.map_ok(move |update| {
                match update {
                    Update::Add(adds) => Update::Add(
                        adds.clone()
                            .into_iter()
                            .map(|(sa, _)| (sa, Target::new(sa, identity.clone())))
                            .collect(),
                    ),
                    Update::Remove(removes) => Update::Remove(removes),
                    Update::Reset(rst) => Update::Reset(rst),
                    Update::DoesNotExist => Update::DoesNotExist,
                }
            }))))
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
