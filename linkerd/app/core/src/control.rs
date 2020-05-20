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
    use futures_03::{ready, TryFuture};
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

/// Resolves the controller's `addr` once before building a client.
pub mod resolve {
    use super::{client, ControlAddr};
    use crate::svc;
    use futures_03::{ready, TryFuture};
    use linkerd2_addr::Addr;
    use linkerd2_dns as dns;
    use pin_project::{pin_project, project};
    use std::future::Future;
    use std::net::SocketAddr;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::{error, fmt};

    #[derive(Clone, Debug)]
    pub struct Layer {
        dns: dns::Resolver,
    }

    #[derive(Clone, Debug)]
    pub struct Resolve<M> {
        dns: dns::Resolver,
        inner: M,
    }

    #[pin_project]
    pub struct Init<M>
    where
        M: tower::Service<client::Target>,
    {
        #[pin]
        state: State<M>,
    }

    #[pin_project]
    enum State<M>
    where
        M: tower::Service<client::Target>,
    {
        Resolve(#[pin] dns::IpAddrFuture, Option<(M, ControlAddr)>),
        NotReady(M, Option<(SocketAddr, ControlAddr)>),
        Inner(#[pin] M::Future),
    }

    #[derive(Debug)]
    pub enum Error<I> {
        Dns(dns::Error),
        Inner(I),
    }

    // === impl Layer ===

    pub fn layer<M>(dns: dns::Resolver) -> impl svc::Layer<M, Service = Resolve<M>> + Clone
    where
        M: tower::Service<client::Target> + Clone,
    {
        svc::layer::mk(move |inner| Resolve {
            dns: dns.clone(),
            inner,
        })
    }

    // === impl Resolve ===

    impl<M> tower::Service<ControlAddr> for Resolve<M>
    where
        M: tower::Service<client::Target> + Clone,
    {
        type Response = M::Response;
        type Error = <Init<M> as TryFuture>::Error;
        type Future = Init<M>;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.inner.poll_ready(cx).map_err(Error::Inner)
        }

        fn call(&mut self, target: ControlAddr) -> Self::Future {
            let state = match target.addr {
                Addr::Socket(sa) => State::make_inner(sa, &target, &mut self.inner),
                Addr::Name(ref na) => {
                    // The inner service is ready, but we are going to do
                    // additional work before using it. In case the inner
                    // service has acquired resources (like a lock), we
                    // relinquish our claim on the service by replacing it.
                    self.inner = self.inner.clone();

                    let future = self.dns.resolve_one_ip(na.name());
                    State::Resolve(future, Some((self.inner.clone(), target.clone())))
                }
            };

            Init { state }
        }
    }

    // === impl Init ===

    impl<M> Future for Init<M>
    where
        M: tower::Service<client::Target>,
    {
        type Output = Result<M::Response, Error<M::Error>>;

        #[project]
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let mut this = self.project();
            loop {
                #[project]
                match this.state.as_mut().project() {
                    State::Resolve(fut, stack) => {
                        let ip = ready!(fut.poll(cx).map_err(Error::Dns))?;
                        let (svc, config) = stack.take().unwrap();
                        let addr = SocketAddr::from((ip, config.addr.port()));
                        this.state
                            .as_mut()
                            .set(State::NotReady(svc, Some((addr, config))));
                    }
                    State::NotReady(svc, cfg) => {
                        ready!(svc.poll_ready(cx).map_err(Error::Inner))?;
                        let (addr, config) = cfg.take().unwrap();
                        let state = State::make_inner(addr, &config, svc);
                        this.state.as_mut().set(state);
                    }
                    State::Inner(fut) => return fut.poll(cx).map_err(Error::Inner),
                };
            }
        }
    }

    impl<M> State<M>
    where
        M: tower::Service<client::Target>,
    {
        fn make_inner(addr: SocketAddr, dst: &ControlAddr, mk_svc: &mut M) -> Self {
            let target = client::Target {
                addr,
                server_name: dst.identity.clone(),
            };

            State::Inner(mk_svc.call(target))
        }
    }

    // === impl Error ===

    impl<I: fmt::Display> fmt::Display for Error<I> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Error::Dns(dns::Error::NoAddressesFound) => write!(f, "no addresses found"),
                Error::Dns(dns::Error::ResolutionFailed(e)) => fmt::Display::fmt(&e, f),
                Error::Inner(ref e) => fmt::Display::fmt(&e, f),
            }
        }
    }

    impl<I: fmt::Debug + fmt::Display> error::Error for Error<I> {}
}

/// Creates a client suitable for gRPC.
pub mod client {
    use crate::transport::{connect, tls};
    use crate::{proxy::http, svc};
    use linkerd2_proxy_http::h2::Settings as H2Settings;
    use std::net::SocketAddr;
    use std::task::{Context, Poll};

    #[derive(Clone, Debug)]
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
