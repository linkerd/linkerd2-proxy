use futures::{future, Async, Future, Poll};
use http;
use hyper;
use std::{error, fmt};
use std::marker::PhantomData;
use tokio::executor::Executor;

use super::{h1, h2, Settings};
use super::glue::{Error, HttpBody, HyperConnect};
use super::normalize_uri::ShouldNormalizeUri;
use super::upgrade::{HttpConnect, Http11Upgrade};
use svc::{self, stack_per_request::ShouldStackPerRequest};
use transport::connect;

/// Configurs an HTTP Client `Service` `Stack`.
///
/// `settings` determines whether an HTTP/1 or HTTP/2 client is used.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Config {
    pub target: connect::Target,
    pub settings: Settings,
    _p: (),
}

/// Configurs an HTTP client that uses a `C`-typed connector
///
/// The `proxy_name` is used for diagnostics (logging, mostly).
#[derive(Debug)]
pub struct Layer<B> {
    proxy_name: &'static str,
    _p: PhantomData<fn() -> B>,
}

/// Configurs an HTTP client that uses a `C`-typed connector
///
/// The `proxy_name` is used for diagnostics (logging, mostly).
#[derive(Debug)]
pub struct Stack<C, B>
where
    C: svc::Stack<connect::Target>,
    C::Value: connect::Connect + Clone + Send + Sync + 'static,
    B: hyper::body::Payload + 'static,
{
    connect: C,
    proxy_name: &'static str,
    _p: PhantomData<fn() -> B>,
}

type HyperClient<C, B> =
    hyper::Client<HyperConnect<C>, B>;

/// A `NewService` that can speak either HTTP/1 or HTTP/2.
pub struct Client<C, B>
where
    B: hyper::body::Payload + 'static,
    C: connect::Connect + 'static,
{
    inner: ClientInner<C, B>,
}

enum ClientInner<C, B> {
    Http1(HyperClient<C, B>),
    Http2(h2::Connect<C, B>),
}

/// A `Future` returned from `Client::new_service()`.
pub enum ClientNewServiceFuture<C, B>
where
    B: hyper::body::Payload + 'static,
    C: connect::Connect + 'static,
{
    Http1(Option<HyperClient<C, B>>),
    Http2(h2::ConnectFuture<C, B>),
}

/// The `Service` yielded by `Client::new_service()`.
pub enum ClientService<C, B>
where
    B: hyper::body::Payload + 'static,
    C: connect::Connect,
{
    Http1(HyperClient<C, B>),
    Http2(h2::Connection<B>),
}

pub enum ClientServiceFuture {
    Http1 {
        future: hyper::client::ResponseFuture,
        upgrade: Option<Http11Upgrade>,
        is_http_connect: bool,
    },
    Http2(h2::ResponseFuture),
}

// === impl Config ===

impl Config {
    pub fn new(target: connect::Target, settings: Settings) -> Self {
        Config { target, settings, _p: () }
    }
}

impl ShouldNormalizeUri for Config {
    fn should_normalize_uri(&self) -> bool {
        !self.settings.is_http2() && !self.settings.was_absolute_form()
    }
}

impl ShouldStackPerRequest for Config {
    fn should_stack_per_request(&self) -> bool {
        !self.settings.is_http2() && !self.settings.can_reuse_clients()
    }
}

impl fmt::Display for Config {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.target.addr.fmt(f)
    }
}


// === impl Layer ===

pub fn layer<B>(proxy_name: &'static str) -> Layer<B>
where
    B: hyper::body::Payload + Send + 'static,
{
    Layer {
        proxy_name,
        _p: PhantomData,
    }
}

impl<B> Clone for Layer<B>
where
    B: hyper::body::Payload + Send + 'static,
{
    fn clone(&self) -> Self {
        Self {
            proxy_name: self.proxy_name,
            _p: PhantomData,
        }
    }
}

impl<C, B> svc::Layer<Config, connect::Target, C> for Layer<B>
where
    C: svc::Stack<connect::Target>,
    C::Value: connect::Connect + Clone + Send + Sync + 'static,
    <C::Value as connect::Connect>::Connected: Send,
    <C::Value as connect::Connect>::Future: Send + 'static,
    <C::Value as connect::Connect>::Error: error::Error + Send + Sync,
    B: hyper::body::Payload + Send + 'static,
{
    type Value = <Stack<C, B> as svc::Stack<Config>>::Value;
    type Error = <Stack<C, B> as svc::Stack<Config>>::Error;
    type Stack = Stack<C, B>;

    fn bind(&self, connect: C) -> Self::Stack {
        Stack {
            connect,
            proxy_name: self.proxy_name,
            _p: PhantomData,
         }
    }
}

// === impl Stack ===

impl<C, B> Clone for Stack<C, B>
where
    C: svc::Stack<connect::Target> + Clone,
    C::Value: connect::Connect + Clone + Send + Sync + 'static,
    B: hyper::body::Payload + Send + 'static,
{
    fn clone(&self) -> Self {
        Self {
            proxy_name: self.proxy_name,
            connect: self.connect.clone(),
            _p: PhantomData,
        }
    }
}

impl<C, B> svc::Stack<Config> for Stack<C, B>
where
    C: svc::Stack<connect::Target>,
    C::Value: connect::Connect + Clone + Send + Sync + 'static,
    <C::Value as connect::Connect>::Connected: Send,
    <C::Value as connect::Connect>::Future: Send + 'static,
    <C::Value as connect::Connect>::Error: error::Error + Send + Sync,
    B: hyper::body::Payload + Send + 'static,
{
    type Value = Client<C::Value, B>;
    type Error = C::Error;

    fn make(&self, config: &Config) -> Result<Self::Value, Self::Error> {
        debug!("building client={:?}", config);
        let connect = self.connect.make(&config.target)?;
        let executor = ::logging::Client::proxy(self.proxy_name, config.target.addr)
            .with_settings(config.settings.clone())
            .executor();
        Ok(Client::new(&config.settings, connect, executor))
    }
}

// === impl Client ===

impl<C, B> Client<C, B>
where
    C: connect::Connect + Clone + Send + Sync + 'static,
    C::Future: Send + 'static,
    C::Error: error::Error + Send + Sync,
    C::Connected: Send,
    B: hyper::body::Payload + 'static,
{
    /// Create a new `Client`, bound to a specific protocol (HTTP/1 or HTTP/2).
    pub fn new<E>(settings: &Settings, connect: C, executor: E) -> Self
    where
        E: Executor + Clone,
        E: future::Executor<Box<Future<Item = (), Error = ()> + Send + 'static>> + Send + Sync + 'static,
    {
        match settings {
            Settings::Http1 { was_absolute_form, .. } => {
                let h1 = hyper::Client::builder()
                    .executor(executor)
                    // hyper should never try to automatically set the Host
                    // header, instead always just passing whatever we received.
                    .set_host(false)
                    .build(HyperConnect::new(connect, *was_absolute_form));
                Client {
                    inner: ClientInner::Http1(h1),
                }
            },
            Settings::Http2 => {
                let h2 = h2::Connect::new(connect, executor);
                Client {
                    inner: ClientInner::Http2(h2),
                }
            }
        }
    }
}

impl<C, B> svc::Service<()> for Client<C, B>
where
    C: connect::Connect + Clone + Send + Sync + 'static,
    C::Future: Send + 'static,
    <C::Future as Future>::Error: error::Error + Send + Sync,
    C::Connected: Send,
    B: hyper::body::Payload + 'static,
{
    type Response = ClientService<C, B>;
    type Error = h2::ConnectError<C::Error>;
    type Future = ClientNewServiceFuture<C, B>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into())
    }

    fn call(&mut self, _target: ()) -> Self::Future {
        match self.inner {
            ClientInner::Http1(ref h1) => {
                ClientNewServiceFuture::Http1(Some(h1.clone()))
            },
            ClientInner::Http2(ref mut h2) => {
                ClientNewServiceFuture::Http2(h2.call(()))
            },
        }
    }
}

// === impl ClientNewServiceFuture ===

impl<C, B> Future for ClientNewServiceFuture<C, B>
where
    C: connect::Connect + Send + 'static,
    C::Connected: Send,
    C::Future: Send + 'static,
    B: hyper::body::Payload + 'static,
{
    type Item = ClientService<C, B>;
    type Error = h2::ConnectError<C::Error>;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let svc = match *self {
            ClientNewServiceFuture::Http1(ref mut h1) => {
                ClientService::Http1(h1.take().expect("poll more than once"))
            },
            ClientNewServiceFuture::Http2(ref mut h2) => {
                let svc = try_ready!(h2.poll());
                ClientService::Http2(svc)
            }
        };
        Ok(Async::Ready(svc))
    }
}

// === impl ClientService ===

impl<C, B> svc::Service<http::Request<B>> for ClientService<C, B>
where
    C: connect::Connect + Send + Sync + 'static,
    C::Connected: Send,
    C::Future: Send + 'static,
    <C::Future as Future>::Error: error::Error + Send + Sync,
    B: hyper::body::Payload + 'static,
{
    type Response = http::Response<HttpBody>;
    type Error = Error;
    type Future = ClientServiceFuture;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        match *self {
            ClientService::Http1(_) => Ok(Async::Ready(())),
            ClientService::Http2(ref mut h2) => h2.poll_ready(),
        }
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        debug!("client request: method={} uri={} version={:?} headers={:?}",
            req.method(), req.uri(), req.version(), req.headers());
        match *self {
            ClientService::Http1(ref h1) => {
                let upgrade = req.extensions_mut().remove::<Http11Upgrade>();
                let is_http_connect = if upgrade.is_some() {
                    req.method() == &http::Method::CONNECT
                } else {
                    false
                };
                ClientServiceFuture::Http1 {
                    future: h1.request(req),
                    upgrade,
                    is_http_connect,
                }
            },
            ClientService::Http2(ref mut h2) => {
                ClientServiceFuture::Http2(h2.call(req))
            }
        }
    }
}

// === impl ClientServiceFuture ===

impl Future for ClientServiceFuture {
    type Item = http::Response<HttpBody>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self {
            ClientServiceFuture::Http1 { future, upgrade, is_http_connect } => {
                let mut res = try_ready!(future.poll())
                    .map(|b| HttpBody {
                        body: Some(b),
                        upgrade: upgrade.take(),
                    });
                if *is_http_connect {
                    res.extensions_mut().insert(HttpConnect);
                }

                if h1::is_upgrade(&res) {
                    trace!("client response is HTTP/1.1 upgrade");
                } else {
                    h1::strip_connection_headers(res.headers_mut());
                }
                Ok(Async::Ready(res))
            },
            ClientServiceFuture::Http2(f) => {
                f.poll()
            },
        }
    }
}

