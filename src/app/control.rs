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
    use bytes::Bytes;
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

    // === impl Layer ===

    pub fn layer<M>() -> Layer<M>
    where
        M: svc::Stack<ControlAddr>,
    {
        Layer { _p: PhantomData }
    }

    impl<M> Clone for Layer<M>
    where
        M: svc::Stack<ControlAddr>,
    {
        fn clone(&self) -> Self {
            layer()
        }
    }

    impl<M> svc::Layer<ControlAddr, ControlAddr, M> for Layer<M>
    where
        M: svc::Stack<ControlAddr>,
    {
        type Value = <Stack<M> as svc::Stack<ControlAddr>>::Value;
        type Error = <Stack<M> as svc::Stack<ControlAddr>>::Error;
        type Stack = Stack<M>;

        fn bind(&self, inner: M) -> Self::Stack {
            Stack { inner }
        }
    }

    // === impl Stack ===

    impl<M> svc::Stack<ControlAddr> for Stack<M>
    where
        M: svc::Stack<ControlAddr>,
    {
        type Value = AddOrigin<M::Value>;
        type Error = M::Error;

        fn make(&self, config: &ControlAddr) -> Result<Self::Value, Self::Error> {
            let inner = self.inner.make(config)?;
            let scheme = uri::Scheme::from_shared(Bytes::from_static(b"http")).unwrap();
            let authority = config.addr.as_authority();
            Ok(AddOrigin::new(inner, scheme, authority))
        }
    }
}

/// Resolves the controller's `addr` once before building a client.
pub mod resolve {
    use futures::{Future, Poll};
    use std::marker::PhantomData;
    use std::net::SocketAddr;
    use std::{error, fmt};

    use super::{client, ControlAddr};
    use dns;
    use svc;
    use Addr;

    #[derive(Debug)]
    pub struct Layer<M> {
        dns: dns::Resolver,
        _p: PhantomData<fn() -> M>,
    }

    #[derive(Clone, Debug)]
    pub struct Stack<M> {
        dns: dns::Resolver,
        inner: M,
    }

    pub struct NewService<M> {
        config: ControlAddr,
        dns: dns::Resolver,
        stack: M,
    }

    pub struct Init<M>
    where
        M: svc::Stack<client::Target>,
        M::Value: svc::Service<()>,
    {
        state: State<M>,
    }

    enum State<M>
    where
        M: svc::Stack<client::Target>,
        M::Value: svc::Service<()>,
    {
        Resolve {
            future: dns::IpAddrFuture,
            config: ControlAddr,
            stack: M,
        },
        Inner(<M::Value as svc::Service<()>>::Future),
        Invalid(Option<M::Error>),
    }

    #[derive(Debug)]
    pub enum Error<S, I> {
        Dns(dns::Error),
        Invalid(S),
        Inner(I),
    }

    // === impl Layer ===

    pub fn layer<M>(dns: dns::Resolver) -> Layer<M>
    where
        M: svc::Stack<client::Target> + Clone,
    {
        Layer {
            dns,
            _p: PhantomData,
        }
    }

    impl<M> Clone for Layer<M>
    where
        M: svc::Stack<client::Target> + Clone,
    {
        fn clone(&self) -> Self {
            layer(self.dns.clone())
        }
    }

    impl<M> svc::Layer<ControlAddr, client::Target, M> for Layer<M>
    where
        M: svc::Stack<client::Target> + Clone,
    {
        type Value = <Stack<M> as svc::Stack<ControlAddr>>::Value;
        type Error = <Stack<M> as svc::Stack<ControlAddr>>::Error;
        type Stack = Stack<M>;

        fn bind(&self, inner: M) -> Self::Stack {
            Stack {
                inner,
                dns: self.dns.clone(),
            }
        }
    }

    // === impl Stack ===

    impl<M> svc::Stack<ControlAddr> for Stack<M>
    where
        M: svc::Stack<client::Target> + Clone,
    {
        type Value = NewService<M>;
        type Error = M::Error;

        fn make(&self, config: &ControlAddr) -> Result<Self::Value, Self::Error> {
            Ok(NewService {
                dns: self.dns.clone(),
                config: config.clone(),
                stack: self.inner.clone(),
            })
        }
    }

    // === impl NewService ===

    impl<M> svc::Service<()> for NewService<M>
    where
        M: svc::Stack<client::Target> + Clone,
        M::Value: svc::Service<()>,
    {
        type Response = <M::Value as svc::Service<()>>::Response;
        type Error = <Init<M> as Future>::Error;
        type Future = Init<M>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            Ok(().into())
        }

        fn call(&mut self, _target: ()) -> Self::Future {
            let state = match self.config.addr {
                Addr::Socket(sa) => State::make_inner(sa, &self.config, &self.stack),
                Addr::Name(ref na) => State::Resolve {
                    future: self.dns.resolve_one_ip(na.name()),
                    stack: self.stack.clone(),
                    config: self.config.clone(),
                },
            };

            Init { state }
        }
    }

    // === impl Init ===

    impl<M> Future for Init<M>
    where
        M: svc::Stack<client::Target>,
        M::Value: svc::Service<()>,
    {
        type Item = <M::Value as svc::Service<()>>::Response;
        type Error = Error<M::Error, <M::Value as svc::Service<()>>::Error>;

        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            loop {
                self.state = match self.state {
                    State::Inner(ref mut fut) => {
                        return fut.poll().map_err(Error::Inner);
                    }
                    State::Resolve {
                        ref mut future,
                        ref config,
                        ref stack,
                    } => {
                        let ip = try_ready!(future.poll().map_err(Error::Dns));
                        let sa = SocketAddr::from((ip, config.addr.port()));
                        State::make_inner(sa, &config, &stack)
                    }
                    State::Invalid(ref mut e) => {
                        return Err(Error::Invalid(
                            e.take().expect("future polled after failure"),
                        ));
                    }
                };
            }
        }
    }

    impl<M> State<M>
    where
        M: svc::Stack<client::Target>,
        M::Value: svc::Service<()>,
    {
        fn make_inner(addr: SocketAddr, dst: &ControlAddr, stack: &M) -> Self {
            let target = client::Target {
                addr,
                server_name: dst.identity.clone(),
                log_ctx: ::logging::admin().client("control", dst.addr.clone()),
            };

            match stack.make(&target) {
                Ok(mut n) => State::Inner(svc::Service::call(&mut n, ())),
                Err(e) => State::Invalid(Some(e)),
            }
        }
    }

    // === impl Error ===

    impl<S: fmt::Display, I: fmt::Display> fmt::Display for Error<S, I> {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            match self {
                Error::Dns(dns::Error::NoAddressesFound) => write!(f, "no addresses found"),
                Error::Dns(dns::Error::ResolutionFailed(e)) => fmt::Display::fmt(&e, f),
                Error::Invalid(ref e) => fmt::Display::fmt(&e, f),
                Error::Inner(ref e) => fmt::Display::fmt(&e, f),
            }
        }
    }

    impl<S: error::Error, I: error::Error> error::Error for Error<S, I> {}
}

/// Creates a client suitable for gRPC.
pub mod client {
    use hyper::body::Payload;
    use std::error;
    use std::marker::PhantomData;
    use std::net::SocketAddr;

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
    pub struct Layer<C, B> {
        _p: PhantomData<(fn() -> C, fn() -> B)>,
    }

    #[derive(Debug)]
    pub struct Stack<C, B> {
        connect: C,
        _p: PhantomData<fn() -> B>,
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

    pub fn layer<C, B>() -> Layer<C, B>
    where
        C: svc::Stack<Target> + Clone,
        C::Value: connect::Connect + Clone + Send + Sync + 'static,
        <C::Value as connect::Connect>::Connected: Send + 'static,
        <C::Value as connect::Connect>::Future: Send + 'static,
        <C::Value as connect::Connect>::Error: Into<Box<dyn error::Error + Send + Sync + 'static>>,
        B: Payload,
    {
        Layer { _p: PhantomData }
    }

    impl<C, B> Clone for Layer<C, B> {
        fn clone(&self) -> Self {
            Layer { _p: PhantomData }
        }
    }

    impl<C, B> svc::Layer<Target, Target, C> for Layer<C, B>
    where
        C: svc::Stack<Target> + Clone,
        C::Value: connect::Connect + Clone + Send + Sync + 'static,
        <C::Value as connect::Connect>::Connected: Send + 'static,
        <C::Value as connect::Connect>::Future: Send + 'static,
        <C::Value as connect::Connect>::Error: Into<Box<dyn error::Error + Send + Sync + 'static>>,
        B: Payload,
    {
        type Value = <Stack<C, B> as svc::Stack<Target>>::Value;
        type Error = <Stack<C, B> as svc::Stack<Target>>::Error;
        type Stack = Stack<C, B>;

        fn bind(&self, connect: C) -> Self::Stack {
            Stack {
                connect,
                _p: PhantomData,
            }
        }
    }

    // === impl Stack ===

    impl<C, B> Clone for Stack<C, B>
    where
        C: Clone,
    {
        fn clone(&self) -> Self {
            Stack {
                connect: self.connect.clone(),
                _p: PhantomData,
            }
        }
    }

    impl<C, B> svc::Stack<Target> for Stack<C, B>
    where
        C: svc::Stack<Target> + Clone,
        C::Value: connect::Connect + Clone + Send + Sync + 'static,
        <C::Value as connect::Connect>::Connected: Send + 'static,
        <C::Value as connect::Connect>::Future: Send + 'static,
        <C::Value as connect::Connect>::Error: Into<Box<dyn error::Error + Send + Sync + 'static>>,
        B: Payload,
    {
        type Value = http::h2::Connect<C::Value, B>;
        type Error = C::Error;

        fn make(&self, target: &Target) -> Result<Self::Value, Self::Error> {
            let c = self.connect.make(&target)?;
            let e = target.log_ctx.clone().with_remote(target.addr).executor();
            Ok(http::h2::Connect::new(c, e))
        }
    }
}
