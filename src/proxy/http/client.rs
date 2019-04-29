use futures::{Async, Future, Poll};
use http;
use hyper;
use std::marker::PhantomData;
use std::{error, fmt};

use super::glue::{Error, HttpBody, HyperConnect};
use super::normalize_uri::ShouldNormalizeUri;
use super::upgrade::{Http11Upgrade, HttpConnect};
use super::{h1, h2, Settings};
use app::config::H2Settings;
use svc::{self, ServiceExt};
use transport::{connect, tls};

/// Configurs an HTTP Client `Service` `Stack`.
///
/// `settings` determines whether an HTTP/1 or HTTP/2 client is used.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Config<T> {
    pub target: T,
    pub settings: Settings,
    _p: (),
}

/// Configurs an HTTP client that uses a `C`-typed connector
///
/// The `proxy_name` is used for diagnostics (logging, mostly).
#[derive(Debug)]
pub struct Layer<T, B> {
    proxy_name: &'static str,
    h2_settings: H2Settings,
    _p: PhantomData<fn(T) -> B>,
}

type HyperClient<C, T, B> = hyper::Client<HyperConnect<C, T>, B>;

/// A `MakeService` that can speak either HTTP/1 or HTTP/2.
pub struct Client<C, T, B> {
    connect: C,
    proxy_name: &'static str,
    h2_settings: H2Settings,
    _p: PhantomData<fn(T) -> B>,
}

/// A `Future` returned from `Client::new_service()`.
pub enum ClientNewServiceFuture<C, T, B>
where
    B: hyper::body::Payload + 'static,
    C: svc::MakeConnection<T> + 'static,
    C::Connection: tls::HasStatus + Send + 'static,
{
    Http1(Option<HyperClient<C, T, B>>),
    Http2(::tower_util::Oneshot<h2::Connect<C, B>, T>),
}

/// The `Service` yielded by `Client::new_service()`.
pub enum ClientService<C, T, B>
where
    B: hyper::body::Payload + 'static,
    C: svc::MakeConnection<T> + 'static,
{
    Http1(HyperClient<C, T, B>),
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

impl<T> Config<T> {
    pub fn new(target: T, settings: Settings) -> Self {
        Config {
            target,
            settings,
            _p: (),
        }
    }
}

impl<T> ShouldNormalizeUri for Config<T> {
    fn should_normalize_uri(&self) -> bool {
        !self.settings.is_http2() && !self.settings.was_absolute_form()
    }
}

impl<T: fmt::Display> fmt::Display for Config<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(&self.target, f)
    }
}

// === impl Layer ===

pub fn layer<T, B>(proxy_name: &'static str, h2_settings: H2Settings) -> Layer<T, B>
where
    B: hyper::body::Payload + Send + 'static,
{
    Layer {
        proxy_name,
        h2_settings,
        _p: PhantomData,
    }
}

impl<T, B> Clone for Layer<T, B>
where
    B: hyper::body::Payload + Send + 'static,
{
    fn clone(&self) -> Self {
        Self {
            proxy_name: self.proxy_name,
            h2_settings: self.h2_settings,
            _p: PhantomData,
        }
    }
}

impl<T, C, B> svc::Layer<C> for Layer<T, B>
where
    Client<C, T, B>: svc::Service<Config<T>>,
    B: hyper::body::Payload + Send + 'static,
{
    type Service = Client<C, T, B>;

    fn layer(&self, connect: C) -> Self::Service {
        Client {
            connect,
            proxy_name: self.proxy_name,
            h2_settings: self.h2_settings,
            _p: PhantomData,
        }
    }
}

// === impl Client ===

/// MakeService
impl<C, T, B> svc::Service<Config<T>> for Client<C, T, B>
where
    C: svc::MakeConnection<T> + Clone + Send + Sync + 'static,
    C::Future: Send + 'static,
    <C::Future as Future>::Error: Into<Box<dyn error::Error + Send + Sync>>,
    C::Connection: tls::HasStatus + Send + 'static,
    T: connect::HasPeerAddr + fmt::Debug + Clone + Send + Sync,
    B: hyper::body::Payload + 'static,
{
    type Response = ClientService<C, T, B>;
    type Error = h2::ConnectError<C::Error>;
    type Future = ClientNewServiceFuture<C, T, B>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into())
    }

    fn call(&mut self, config: Config<T>) -> Self::Future {
        debug!("building client={:?}", config);

        let connect = self.connect.clone();
        let executor = ::logging::Client::proxy(self.proxy_name, config.target.peer_addr())
            .with_settings(config.settings.clone())
            .executor();

        match config.settings {
            Settings::Http1 {
                keep_alive,
                was_absolute_form,
            } => {
                let h1 = hyper::Client::builder()
                    .executor(executor)
                    .keep_alive(keep_alive)
                    // hyper should never try to automatically set the Host
                    // header, instead always just passing whatever we received.
                    .set_host(false)
                    .build(HyperConnect::new(connect, config.target, was_absolute_form));
                ClientNewServiceFuture::Http1(Some(h1))
            }
            Settings::Http2 => {
                let h2 = h2::Connect::new(connect, executor, self.h2_settings.clone())
                    .oneshot(config.target);
                ClientNewServiceFuture::Http2(h2)
            }
        }
    }
}

impl<C, T, B> Clone for Client<C, T, B>
where
    C: Clone,
{
    fn clone(&self) -> Self {
        Client {
            connect: self.connect.clone(),
            proxy_name: self.proxy_name,
            h2_settings: self.h2_settings,
            _p: PhantomData,
        }
    }
}

// === impl ClientNewServiceFuture ===

impl<C, T, B> Future for ClientNewServiceFuture<C, T, B>
where
    C: svc::MakeConnection<T> + Send + Sync + 'static,
    C::Connection: tls::HasStatus + Send + 'static,
    C::Future: Send + 'static,
    B: hyper::body::Payload + 'static,
{
    type Item = ClientService<C, T, B>;
    type Error = h2::ConnectError<C::Error>;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let svc = match *self {
            ClientNewServiceFuture::Http1(ref mut h1) => {
                ClientService::Http1(h1.take().expect("poll more than once"))
            }
            ClientNewServiceFuture::Http2(ref mut h2) => {
                let svc = try_ready!(h2.poll());
                ClientService::Http2(svc)
            }
        };
        Ok(Async::Ready(svc))
    }
}

// === impl ClientService ===

impl<C, T, B> svc::Service<http::Request<B>> for ClientService<C, T, B>
where
    C: svc::MakeConnection<T> + Clone + Send + Sync + 'static,
    C::Connection: tls::HasStatus + Send,
    C::Future: Send + 'static,
    <C::Future as Future>::Error: Into<Box<dyn error::Error + Send + Sync>>,
    T: Clone + Send + Sync + 'static,
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
        debug!(
            "client request: method={} uri={} version={:?} headers={:?}",
            req.method(),
            req.uri(),
            req.version(),
            req.headers()
        );
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
            }
            ClientService::Http2(ref mut h2) => ClientServiceFuture::Http2(h2.call(req)),
        }
    }
}

// === impl ClientServiceFuture ===

impl Future for ClientServiceFuture {
    type Item = http::Response<HttpBody>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self {
            ClientServiceFuture::Http1 {
                future,
                upgrade,
                is_http_connect,
            } => {
                let mut res = try_ready!(future.poll()).map(|b| HttpBody {
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
            }
            ClientServiceFuture::Http2(f) => f.poll(),
        }
    }
}
