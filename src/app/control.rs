use h2;
use std::fmt;
use std::time::Duration;

use svc;
use transport::tls;
use {Conditional, Addr};

#[derive(Clone, Debug)]
pub struct Config {
    addr: Addr,
    tls_server_identity: Conditional<tls::Identity, tls::ReasonForNoTls>,
    tls_config: tls::ConditionalClientConfig,
    backoff: Duration,
    connect_timeout: Duration,
    builder: h2::client::Builder,
}

impl Config {
    pub fn new(
        addr: Addr,
        tls_server_identity: Conditional<tls::Identity, tls::ReasonForNoTls>,
        backoff: Duration,
        connect_timeout: Duration,
    ) -> Self {
        Self {
            addr,
            tls_server_identity,
            tls_config: Conditional::None(tls::ReasonForNoTls::Disabled),
            backoff,
            connect_timeout,
            builder: h2::client::Builder::default(),
        }
    }

    pub fn addr(&self) -> &Addr {
        &self.addr
    }

    pub fn tls_status(&self) -> tls::Status {
        self.tls_config.as_ref().map(|_| ())
    }
}

impl svc::watch::WithUpdate<tls::ConditionalClientConfig> for Config {
    type Updated = Self;

    fn with_update(&self, tls_config: &tls::ConditionalClientConfig) -> Self::Updated {
        let mut c = self.clone();
        c.tls_config = tls_config.clone();
        c
    }
}

impl fmt::Display for Config {
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
        M: svc::Stack<super::Config>,
    {
        Layer { _p: PhantomData }
    }

    impl<M> Clone for Layer<M>
    where
        M: svc::Stack<super::Config>,
    {
        fn clone(&self) -> Self {
            layer()
        }
    }

    impl<M> svc::Layer<super::Config, super::Config, M> for Layer<M>
    where
        M: svc::Stack<super::Config>,
    {
        type Value = <Stack<M> as svc::Stack<super::Config>>::Value;
        type Error = <Stack<M> as svc::Stack<super::Config>>::Error;
        type Stack = Stack<M>;

        fn bind(&self, inner: M) -> Self::Stack {
            Stack { inner }
        }
    }

    // === impl Stack ===

    impl<M> svc::Stack<super::Config> for Stack<M>
    where
        M: svc::Stack<super::Config>,
    {
        type Value = AddOrigin<M::Value>;
        type Error = M::Error;

        fn make(&self, config: &super::Config) -> Result<Self::Value, Self::Error> {
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

    use super::client;
    use dns;
    use svc;
    use transport::{connect, tls};
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
        config: super::Config,
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
            config: super::Config,
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

    impl<M> svc::Layer<super::Config, client::Target, M> for Layer<M>
    where
        M: svc::Stack<client::Target> + Clone,
    {
        type Value = <Stack<M> as svc::Stack<super::Config>>::Value;
        type Error = <Stack<M> as svc::Stack<super::Config>>::Error;
        type Stack = Stack<M>;

        fn bind(&self, inner: M) -> Self::Stack {
            Stack {
                inner,
                dns: self.dns.clone(),
            }
        }
    }

    // === impl Stack ===

    impl<M> svc::Stack<super::Config> for Stack<M>
    where
        M: svc::Stack<client::Target> + Clone,
    {
        type Value = NewService<M>;
        type Error = M::Error;

        fn make(&self, config: &super::Config) -> Result<Self::Value, Self::Error> {
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
                        return Err(Error::Invalid(e.take().expect("future polled after failure")));
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
        fn make_inner(addr: SocketAddr, config: &super::Config, stack: &M) -> Self {
            let tls = config.tls_server_identity.as_ref().and_then(|id| {
                config
                    .tls_config
                    .as_ref()
                    .map(|config| tls::ConnectionConfig {
                        server_identity: id.clone(),
                        config: config.clone(),
                    })
            });

            let target = client::Target {
                connect: connect::Target::new(addr, tls),
                builder: config.builder.clone(),
                log_ctx: ::logging::admin().client("control", config.addr.clone()),
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
    use h2;
    use std::marker::PhantomData;
    use tower_h2::client;
    use tower_grpc::BoxBody;

    use svc;
    use transport::connect;
    use Addr;

    #[derive(Clone, Debug)]
    pub struct Target {
        pub(super) connect: connect::Target,
        pub(super) builder: h2::client::Builder,
        pub(super) log_ctx: ::logging::Client<&'static str, Addr>,
    }

    #[derive(Debug)]
    pub struct Layer<C> {
        _p: PhantomData<fn() -> C>,
    }

    #[derive(Clone, Debug)]
    pub struct Stack<C> {
        connect: C,
    }

    // === impl Layer ===

    pub fn layer<C>() -> Layer<C>
    where
        C: svc::Stack<connect::Target> + Clone,
        C::Value: connect::Connect,
        <C::Value as connect::Connect>::Connected: Send + 'static,
    {
        Layer { _p: PhantomData }
    }

    impl<C> Clone for Layer<C>
    where
        C: svc::Stack<connect::Target> + Clone,
        C::Value: connect::Connect,
        <C::Value as connect::Connect>::Connected: Send + 'static,
    {
        fn clone(&self) -> Self {
            layer()
        }
    }

    impl<C> svc::Layer<Target, connect::Target, C> for Layer<C>
    where
        C: svc::Stack<connect::Target> + Clone,
        C::Value: connect::Connect,
        <C::Value as connect::Connect>::Connected: Send + 'static,
    {
        type Value = <Stack<C> as svc::Stack<Target>>::Value;
        type Error = <Stack<C> as svc::Stack<Target>>::Error;
        type Stack = Stack<C>;

        fn bind(&self, connect: C) -> Self::Stack {
            Stack { connect }
        }
    }

    // === impl Stack ===

    impl<C> svc::Stack<Target> for Stack<C>
    where
        C: svc::Stack<connect::Target> + Clone,
        C::Value: connect::Connect,
        <C::Value as connect::Connect>::Connected: Send + 'static,
    {
        type Value = client::Connect<
            C::Value,
            ::logging::ClientExecutor<&'static str, Addr>,
            BoxBody,
        >;
        type Error = C::Error;

        fn make(&self, target: &Target) -> Result<Self::Value, Self::Error> {
            let c = self.connect.make(&target.connect)?;
            let h2 = target.builder.clone();
            let e = target
                .log_ctx
                .clone()
                .with_remote(target.connect.addr)
                .executor();
            Ok(client::Connect::new(c, h2, e))
        }
    }
}

pub mod box_request_body {
    use bytes::Bytes;
    use http;
    use futures::Poll;
    use std::marker::PhantomData;
    use tower_grpc::{Body, BoxBody};

    use svc;

    #[derive(Debug)]
    pub struct Layer<B>(PhantomData<fn() -> B>);

    #[derive(Debug)]
    pub struct Stack<B, M> {
        inner: M,
        _p: PhantomData<fn() -> B>,
    }

    #[derive(Debug)]
    pub struct Service<B, S> {
        inner: S,
        _p: PhantomData<fn() -> B>,
    }

    // === impl Layer ===

    pub fn layer<B>() -> Layer<B>
    where
        B: Body + Send + 'static,
    {
        Layer(PhantomData)
    }

    impl<B> Clone for Layer<B> {
        fn clone(&self) -> Self {
            Layer(PhantomData)
        }
    }

    impl<B, T, M> svc::Layer<T, T, M> for Layer<B>
    where
        B: Body<Data = Bytes> + Send + 'static,
        M: svc::Stack<T>,
        M::Value: svc::Service<http::Request<BoxBody>>,
    {
        type Value = <Stack<B, M> as svc::Stack<T>>::Value;
        type Error = <Stack<B, M> as svc::Stack<T>>::Error;
        type Stack = Stack<B, M>;

        fn bind(&self, inner: M) -> Self::Stack {
            Stack { inner, _p: PhantomData }
        }
    }

    // === impl Stack ===

    impl<B, M: Clone> Clone for Stack<B, M> {
        fn clone(&self) -> Self {
            Stack {
                inner: self.inner.clone(),
                _p: PhantomData,
            }
        }
    }

    impl<B, T, M> svc::Stack<T> for Stack<B, M>
    where
        B: Body<Data = Bytes> + Send + 'static,
        M: svc::Stack<T>,
        M::Value: svc::Service<http::Request<BoxBody>>,
    {
        type Value = Service<B, M::Value>;
        type Error = M::Error;

        fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
            let inner = self.inner.make(target)?;
            Ok(Service { inner, _p: PhantomData })
        }
    }

    // === impl Service ===

    impl<B, S> svc::Service<http::Request<B>> for Service<B, S>
    where
        B: Body<Data = Bytes> + Send + 'static,
        S: svc::Service<http::Request<BoxBody>>,
    {
        type Response = S::Response;
        type Error = S::Error;
        type Future = S::Future;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.inner.poll_ready()
        }

        fn call(&mut self, req: http::Request<B>) -> Self::Future {
            let (head, body) = req.into_parts();
            let req = http::Request::from_parts(head, BoxBody::new(Box::new(body)));
            self.inner.call(req)
        }
    }
}
