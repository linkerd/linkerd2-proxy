use crate::{transport::tls, Addr};
use std::fmt;

#[derive(Clone, Debug)]
pub struct ControlAddr {
    pub addr: Addr,
    pub identity: tls::PeerIdentity,
}

impl fmt::Display for ControlAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.addr, f)
    }
}

/// Sets the request's URI from `Config`.
pub mod add_origin {
    use super::ControlAddr;
    use crate::svc;
    use crate::Error;
    use futures::try_ready;
    use futures::{Future, Poll};
    use std::marker::PhantomData;
    use tower_request_modifier::{Builder, RequestModifier};

    #[derive(Debug)]
    pub struct Layer<M, B> {
        _p: PhantomData<fn(B) -> M>,
    }

    #[derive(Debug)]
    pub struct Stack<M, B> {
        inner: M,
        _p: PhantomData<fn(B)>,
    }

    pub struct MakeFuture<F, B> {
        inner: F,
        authority: http::uri::Authority,
        _p: PhantomData<fn(B)>,
    }

    // === impl Layer ===

    pub fn layer<M, B>() -> Layer<M, B>
    where
        M: svc::Service<ControlAddr>,
    {
        Layer { _p: PhantomData }
    }

    impl<M, B> Clone for Layer<M, B>
    where
        M: svc::Service<ControlAddr>,
    {
        fn clone(&self) -> Self {
            layer()
        }
    }

    impl<M, B> svc::Layer<M> for Layer<M, B>
    where
        M: svc::Service<ControlAddr>,
    {
        type Service = Stack<M, B>;

        fn layer(&self, inner: M) -> Self::Service {
            Stack {
                inner,
                _p: PhantomData,
            }
        }
    }

    // === impl Stack ===

    impl<M, B> svc::Service<ControlAddr> for Stack<M, B>
    where
        M: svc::Service<ControlAddr>,
        M::Error: Into<Error>,
    {
        type Response = RequestModifier<M::Response, B>;
        type Error = Error;
        type Future = MakeFuture<M::Future, B>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.inner.poll_ready().map_err(Into::into)
        }

        fn call(&mut self, target: ControlAddr) -> Self::Future {
            let authority = target.addr.as_authority();
            let inner = self.inner.call(target);
            MakeFuture {
                inner,
                authority,
                _p: PhantomData,
            }
        }
    }

    impl<M, B> Clone for Stack<M, B>
    where
        M: svc::Service<ControlAddr> + Clone,
    {
        fn clone(&self) -> Self {
            Self {
                inner: self.inner.clone(),
                _p: PhantomData,
            }
        }
    }

    // === impl MakeFuture ===

    impl<F, B> Future for MakeFuture<F, B>
    where
        F: Future,
        F::Error: Into<Error>,
    {
        type Item = RequestModifier<F::Item, B>;
        type Error = Error;

        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            let inner = try_ready!(self.inner.poll().map_err(Into::into));

            Builder::new()
                .set_origin(format!("http://{}", self.authority))
                .build(inner)
                .map_err(|_| BuildError.into())
                .map(|a| a.into())
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
    use crate::{dns, logging, svc, Addr};
    use futures::{try_ready, Future, Poll};
    use std::net::SocketAddr;
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

    pub struct Init<M>
    where
        M: svc::Service<client::Target>,
    {
        state: State<M>,
    }

    enum State<M>
    where
        M: svc::Service<client::Target>,
    {
        Resolve {
            future: dns::IpAddrFuture,
            config: ControlAddr,
            stack: M,
        },
        Inner(M::Future),
    }

    #[derive(Debug)]
    pub enum Error<I> {
        Dns(dns::Error),
        Inner(I),
    }

    // === impl Layer ===

    pub fn layer<M>(dns: dns::Resolver) -> impl svc::Layer<M, Service = Resolve<M>> + Clone
    where
        M: svc::Service<client::Target> + Clone,
    {
        svc::layer::mk(move |inner| Resolve {
            dns: dns.clone(),
            inner,
        })
    }

    // === impl Resolve ===

    impl<M> svc::Service<ControlAddr> for Resolve<M>
    where
        M: svc::Service<client::Target> + Clone,
    {
        type Response = M::Response;
        type Error = <Init<M> as Future>::Error;
        type Future = Init<M>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.inner.poll_ready().map_err(Error::Inner)
        }

        fn call(&mut self, target: ControlAddr) -> Self::Future {
            let state = match target.addr {
                Addr::Socket(sa) => State::make_inner(sa, &target, &mut self.inner),
                Addr::Name(ref na) => State::Resolve {
                    future: self.dns.resolve_one_ip(na.name()),
                    stack: self.inner.clone(),
                    config: target.clone(),
                },
            };

            Init { state }
        }
    }

    // === impl Init ===

    impl<M> Future for Init<M>
    where
        M: svc::Service<client::Target>,
    {
        type Item = M::Response;
        type Error = Error<M::Error>;

        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            loop {
                self.state = match self.state {
                    State::Inner(ref mut fut) => {
                        return fut.poll().map_err(Error::Inner);
                    }
                    State::Resolve {
                        ref mut future,
                        ref config,
                        ref mut stack,
                    } => {
                        let ip = try_ready!(future.poll().map_err(Error::Dns));
                        let sa = SocketAddr::from((ip, config.addr.port()));
                        State::make_inner(sa, config, stack)
                    }
                };
            }
        }
    }

    impl<M> State<M>
    where
        M: svc::Service<client::Target>,
    {
        fn make_inner(addr: SocketAddr, dst: &ControlAddr, mk_svc: &mut M) -> Self {
            let target = client::Target {
                addr,
                server_name: dst.identity.clone(),
                log_ctx: logging::admin().client("control", dst.addr.clone()),
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
    use super::super::config::H2Settings;
    use crate::transport::{connect, tls};
    use crate::{logging, proxy::http, svc, task, Addr};
    use futures::Poll;
    use std::net::SocketAddr;

    #[derive(Clone, Debug)]
    pub struct Target {
        pub(super) addr: SocketAddr,
        pub(super) server_name: tls::PeerIdentity,
        pub(super) log_ctx: logging::Client<&'static str, Addr>,
    }

    #[derive(Debug)]
    pub struct Client<C, B> {
        inner: http::h2::Connect<C, B>,
    }

    // === impl Target ===

    impl connect::HasPeerAddr for Target {
        fn peer_addr(&self) -> SocketAddr {
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
        http::h2::Connect<C, B>: svc::Service<Target>,
    {
        svc::layer::mk(|mk_conn| {
            let inner = http::h2::Connect::new(mk_conn, task::LazyExecutor, H2Settings::default());
            Client { inner }
        })
    }

    // === impl Client ===

    impl<C, B> svc::Service<Target> for Client<C, B>
    where
        http::h2::Connect<C, B>: svc::Service<Target>,
    {
        type Response = <http::h2::Connect<C, B> as svc::Service<Target>>::Response;
        type Error = <http::h2::Connect<C, B> as svc::Service<Target>>::Error;
        type Future = <http::h2::Connect<C, B> as svc::Service<Target>>::Future;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.inner.poll_ready()
        }

        fn call(&mut self, target: Target) -> Self::Future {
            let exe = target.log_ctx.clone().with_remote(target.addr).executor();
            self.inner.set_executor(exe);
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
