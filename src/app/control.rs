use std::fmt;

use transport::tls;
use Addr;

#[derive(Clone, Debug)]
pub struct ControlAddr {
    pub addr: Addr,
    pub identity: tls::PeerIdentity,
}

impl fmt::Display for ControlAddr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(&self.addr, f)
    }
}

/// Sets the request's URI from `Config`.
pub mod add_origin {
    extern crate tower_add_origin;

    use self::tower_add_origin::AddOrigin;
    use futures::{Future, Poll};
    use http::uri;
    use std::marker::PhantomData;

    use super::ControlAddr;
    use svc;

    #[derive(Debug)]
    pub struct Layer<M> {
        _p: PhantomData<fn() -> M>,
    }

    #[derive(Clone, Debug)]
    pub struct Stack<M> {
        inner: M,
    }

    pub struct MakeFuture<F> {
        authority: uri::Authority,
        inner: F,
    }

    // === impl Layer ===

    pub fn layer<M>() -> Layer<M>
    where
        M: svc::Service<ControlAddr>,
    {
        Layer { _p: PhantomData }
    }

    impl<M> Clone for Layer<M>
    where
        M: svc::Service<ControlAddr>,
    {
        fn clone(&self) -> Self {
            layer()
        }
    }

    impl<M> svc::Layer<M> for Layer<M>
    where
        M: svc::Service<ControlAddr>,
    {
        type Service = Stack<M>;

        fn layer(&self, inner: M) -> Self::Service {
            Stack { inner }
        }
    }

    // === impl Stack ===

    impl<M> svc::Service<ControlAddr> for Stack<M>
    where
        M: svc::Service<ControlAddr>,
    {
        type Response = AddOrigin<M::Response>;
        type Error = M::Error;
        type Future = MakeFuture<M::Future>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.inner.poll_ready()
        }

        fn call(&mut self, target: ControlAddr) -> Self::Future {
            let authority = target.addr.as_authority();
            let inner = self.inner.call(target);
            MakeFuture { authority, inner }
        }
    }

    // === impl MakeFuture ===

    impl<F: Future> Future for MakeFuture<F> {
        type Item = AddOrigin<F::Item>;
        type Error = F::Error;

        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            let inner = try_ready!(self.inner.poll());
            Ok(AddOrigin::new(inner, uri::Scheme::HTTP, self.authority.clone()).into())
        }
    }
}

/// Resolves the controller's `addr` once before building a client.
pub mod resolve {
    use futures::{Future, Poll};
    use std::net::SocketAddr;
    use std::{error, fmt};

    use super::{client, ControlAddr};
    use dns;
    use svc;
    use Addr;

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
                log_ctx: ::logging::admin().client("control", dst.addr.clone()),
            };

            State::Inner(mk_svc.call(target))
        }
    }

    // === impl Error ===

    impl<I: fmt::Display> fmt::Display for Error<I> {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
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
    use std::net::SocketAddr;

    use futures::Poll;

    use super::super::config::H2Settings;
    use proxy::http;
    use svc;
    use transport::{connect, tls};
    use Addr;

    #[derive(Clone, Debug)]
    pub struct Target {
        pub(super) addr: SocketAddr,
        pub(super) server_name: tls::PeerIdentity,
        pub(super) log_ctx: ::logging::Client<&'static str, Addr>,
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
            let inner =
                http::h2::Connect::new(mk_conn, ::task::LazyExecutor, H2Settings::default());
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
