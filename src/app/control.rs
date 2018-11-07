use h2;
use std::fmt;
use std::time::Duration;

use svc;
use transport::tls;
use {Conditional, HostPort};

#[derive(Clone, Debug)]
pub struct Config {
    host_and_port: HostPort,
    tls_server_identity: Conditional<tls::Identity, tls::ReasonForNoTls>,
    tls_config: tls::ConditionalClientConfig,
    backoff: Duration,
    connect_timeout: Duration,
    builder: h2::client::Builder,
}

impl Config {
    pub fn new(
        host_and_port: HostPort,
        tls_server_identity: Conditional<tls::Identity, tls::ReasonForNoTls>,
        backoff: Duration,
        connect_timeout: Duration,
    ) -> Self {
        Self {
            host_and_port,
            tls_server_identity,
            tls_config: Conditional::None(tls::ReasonForNoTls::Disabled),
            backoff,
            connect_timeout,
            builder: h2::client::Builder::default(),
        }
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
        fmt::Display::fmt(&self.host_and_port, f)
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

    impl<M> Layer<M>
    where
        M: svc::Stack<super::Config>,
    {
        pub fn new() -> Self {
            Self { _p: PhantomData }
        }
    }

    impl<M> Clone for Layer<M>
    where
        M: svc::Stack<super::Config>,
    {
        fn clone(&self) -> Self {
            Self::new()
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
            let authority = config.host_and_port.as_authority();
            Ok(AddOrigin::new(inner, scheme, authority))
        }
    }
}

/// Resolves the controller's `host_and_port` once before building a client.
pub mod resolve {
    use futures::{Future, Poll};
    use std::marker::PhantomData;
    use std::net::SocketAddr;
    use std::{error, fmt};

    use super::client;
    use dns;
    use svc;
    use transport::{connect, tls};

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
        M::Value: svc::NewService,
    {
        state: State<M>,
    }

    enum State<M>
    where
        M: svc::Stack<client::Target>,
        M::Value: svc::NewService,
    {
        Resolve {
            future: dns::IpAddrFuture,
            config: super::Config,
            stack: M,
        },
        Inner(<M::Value as svc::NewService>::Future),
    }

    #[derive(Debug)]
    pub enum Error<S, I> {
        Dns(dns::Error),
        Invalid(S),
        Inner(I),
    }

    // === impl Layer ===

    impl<M> Layer<M>
    where
        M: svc::Stack<client::Target> + Clone,
    {
        pub fn new(dns: dns::Resolver) -> Self {
            Self {
                dns,
                _p: PhantomData,
            }
        }
    }

    impl<M> Clone for Layer<M>
    where
        M: svc::Stack<client::Target> + Clone,
    {
        fn clone(&self) -> Self {
            Self::new(self.dns.clone())
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

    impl<M> svc::NewService for NewService<M>
    where
        M: svc::Stack<client::Target> + Clone,
        M::Value: svc::NewService,
    {
        type Request = <M::Value as svc::NewService>::Request;
        type Response = <M::Value as svc::NewService>::Response;
        type Error = <M::Value as svc::NewService>::Error;
        type Service = <M::Value as svc::NewService>::Service;
        type InitError = <Init<M> as Future>::Error;
        type Future = Init<M>;

        fn new_service(&self) -> Self::Future {
            Init {
                state: State::Resolve {
                    future: self.dns.resolve_one_ip(&self.config.host_and_port),
                    stack: self.stack.clone(),
                    config: self.config.clone(),
                },
            }
        }
    }

    // === impl Init ===

    impl<M> Future for Init<M>
    where
        M: svc::Stack<client::Target>,
        M::Value: svc::NewService,
    {
        type Item = <M::Value as svc::NewService>::Service;
        type Error = Error<M::Error, <M::Value as svc::NewService>::InitError>;

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
                        let sa = SocketAddr::from((ip, config.host_and_port.port()));

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
                            connect: connect::Target::new(sa, tls),
                            builder: config.builder.clone(),
                            log_ctx: ::logging::admin()
                                .client("control", config.host_and_port.clone()),
                        };

                        let inner = stack.make(&target).map_err(Error::Invalid)?;
                        State::Inner(svc::NewService::new_service(&inner))
                    }
                };
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
    use tower_h2::{client, BoxBody};

    use svc;
    use transport::connect;
    use HostPort;

    #[derive(Clone, Debug)]
    pub struct Target {
        pub(super) connect: connect::Target,
        pub(super) builder: h2::client::Builder,
        pub(super) log_ctx: ::logging::Client<&'static str, HostPort>,
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

    impl<C> Layer<C>
    where
        C: svc::Stack<connect::Target> + Clone,
        C::Value: connect::Connect,
        <C::Value as connect::Connect>::Connected: Send + 'static,
    {
        pub fn new() -> Self {
            Self { _p: PhantomData }
        }
    }

    impl<C> Clone for Layer<C>
    where
        C: svc::Stack<connect::Target> + Clone,
        C::Value: connect::Connect,
        <C::Value as connect::Connect>::Connected: Send + 'static,
    {
        fn clone(&self) -> Self {
            Self::new()
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
            ::logging::ClientExecutor<&'static str, HostPort>,
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
