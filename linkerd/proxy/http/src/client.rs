use super::glue::{HttpBody, HyperConnect};
use super::upgrade::{Http11Upgrade, HttpConnect};
use super::{
    h1, h2,
    settings::{HasSettings, Settings},
};
use futures_03::{
    compat::{Compat01As03, Future01CompatExt},
    ready,
};
use http;
use hyper;
use linkerd2_error::Error;
use pin_project::{pin_project, project};
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio as tokio_01;
use tower_03::ServiceExt;
use tracing::{debug, info_span, trace};
use tracing_futures::Instrument;

/// Configures an HTTP client that uses a `C`-typed connector
#[derive(Debug)]
pub struct MakeClientLayer<B> {
    h2_settings: crate::h2::Settings,
    _marker: PhantomData<fn() -> B>,
}

type HyperMakeClient<C, T, B> = hyper::Client<HyperConnect<C, T>, B>;

/// A `MakeService` that can speak either HTTP/1 or HTTP/2.
pub struct MakeClient<C, B> {
    connect: C,
    h2_settings: crate::h2::Settings,
    _marker: PhantomData<fn() -> B>,
}

/// A `Future` returned from `MakeClient::new_service()`.
#[pin_project]
pub enum MakeFuture<C, T, B>
where
    B: hyper::body::Payload + 'static,
    C: tower_03::Service<T> + 'static,
    C::Error: Into<Error>,
    C::Response: tokio_01::io::AsyncRead + tokio_01::io::AsyncWrite + Send + 'static,
{
    Http1(Option<HyperMakeClient<C, T, B>>),
    Http2(#[pin] Compat01As03<tower_util::Oneshot<h2::Connect<C, B>, T>>),
}

/// The `Service` yielded by `MakeClient::new_service()`.
pub enum Client<C, T, B>
where
    B: hyper::body::Payload + 'static,
    C: tower::MakeConnection<T> + 'static,
{
    Http1(HyperMakeClient<C, T, B>),
    Http2(h2::Connection<B>),
}

#[pin_project]
pub enum ClientFuture {
    Http1 {
        future: #[pin] Compat01As03<hyper::client::ResponseFuture>,
        upgrade: Option<Http11Upgrade>,
        is_http_connect: bool,
    },
    Http2(#[pin] Compat01As03<h2::ResponseFuture>),
}

// === impl MakeClientLayer ===

impl<B> MakeClientLayer<B> {
    pub fn new(h2_settings: crate::h2::Settings) -> Self {
        Self {
            h2_settings,
            _marker: PhantomData,
        }
    }
}

impl<B> Clone for MakeClientLayer<B>
where
    B: hyper::body::Payload + Send + 'static,
{
    fn clone(&self) -> Self {
        Self {
            h2_settings: self.h2_settings,
            _marker: self._marker,
        }
    }
}

impl<C, B> tower::layer::Layer<C> for MakeClientLayer<B>
where
    B: hyper::body::Payload + Send + 'static,
{
    type Service = MakeClient<C, B>;

    fn layer(&self, connect: C) -> Self::Service {
        MakeClient {
            connect,
            h2_settings: self.h2_settings,
            _marker: PhantomData,
        }
    }
}

// === impl MakeClient ===

impl<C, T, B> tower_03::Service<T> for MakeClient<C, B>
where
    C: tower_03::Service<T> + Clone + Send + Sync + 'static,
    C::Future: Send + 'static,
    C::Error: Into<Error>,
    C::Response: tokio_01::io::AsyncRead + tokio_01::io::AsyncWrite + Send + 'static,
    T: HasSettings + Clone + Send + Sync,
    B: hyper::body::Payload + 'static,
{
    type Response = Client<C, T, B>;
    type Error = Error;
    type Future = MakeFuture<C, T, B>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Ok(().into())
    }

    fn call(&mut self, target: T) -> Self::Future {
        trace!("Building HTTP client");
        let connect = self.connect.clone();
        match target.http_settings() {
            Settings::Http1 {
                keep_alive,
                wants_h1_upgrade: _,
                was_absolute_form,
            } => {
                let exec =
                    tokio::executor::DefaultExecutor::current().instrument(info_span!("http1"));

                let h1 = hyper::Client::builder()
                    .executor(exec)
                    .keep_alive(keep_alive)
                    // hyper should only try to automatically
                    // set the host if the request was in absolute_form
                    .set_host(was_absolute_form)
                    .build(HyperConnect::new(connect, target, was_absolute_form));
                MakeFuture::Http1(Some(h1))
            }
            Settings::Http2 => {
                let h2 = h2::Connect::new(connect, self.h2_settings.clone())
                    .oneshot(target)
                    .compat();
                MakeFuture::Http2(h2)
            }
        }
    }
}

impl<C: Clone, B> Clone for MakeClient<C, B> {
    fn clone(&self) -> Self {
        Self {
            connect: self.connect.clone(),
            h2_settings: self.h2_settings,
            _marker: self._marker,
        }
    }
}

// === impl MakeFuture ===

impl<C, T, B> Future for MakeFuture<C, T, B>
where
    C: tower::MakeConnection<T> + Send + Sync + 'static,
    C::Connection: Send + 'static,
    C::Future: Send + 'static,
    C::Error: Into<Error>,
    B: hyper::body::Payload + 'static,
{
    type Output = Result<Client<C, T, B>, Error>;

    #[project]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        #[project]
        let svc = match self.project() {
            MakeFuture::Http1(h1) => Client::Http1(h1.take().expect("poll more than once")),
            MakeFuture::Http2(h2) => {
                let svc = ready!(h2.poll(cx))?;
                Client::Http2(svc)
            }
        };
        Poll::Ready(Ok(svc))
    }
}

// === impl Client ===

impl<C, T, B> tower::Service<http::Request<B>> for Client<C, T, B>
where
    C: tower::MakeConnection<T> + Clone + Send + Sync + 'static,
    C::Connection: Send,
    C::Future: Send + 'static,
    C::Error: Into<Error>,
    T: Clone + Send + Sync + 'static,
    B: hyper::body::Payload + 'static,
{
    type Response = http::Response<HttpBody>;
    type Error = Error;
    type Future = ClientFuture;

    fn poll_ready(&mut self) -> Poll<Result<(), Self::Error>> {
        match *self {
            Client::Http1(_) => Poll::Ready(Ok(())),
            Client::Http2(ref mut h2) => h2.poll_ready().map_err(Into::into),
        }
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        debug!(
            method = %req.method(),
            uri = %req.uri(),
            version = ?req.version(),
            headers = ?req.headers(),
        );

        match *self {
            Client::Http1(ref h1) => {
                let upgrade = req.extensions_mut().remove::<Http11Upgrade>();
                let is_http_connect = if upgrade.is_some() {
                    req.method() == &http::Method::CONNECT
                } else {
                    false
                };
                ClientFuture::Http1 {
                    future: h1.request(req),
                    upgrade,
                    is_http_connect,
                }
            }
            Client::Http2(ref mut h2) => ClientFuture::Http2(h2.call(req)),
        }
    }
}

// === impl ClientFuture ===

impl Future for ClientFuture {
    type Output = Result<http::Response<HttpBody>, Error>;

    #[project]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        #[project]
        match self.project() {
            ClientFuture::Http1 {
                future,
                upgrade,
                is_http_connect,
            } => {
                let mut res = ready!(future.poll()).map(|b| HttpBody {
                    body: Some(b),
                    upgrade: upgrade.take(),
                })?;
                if *is_http_connect {
                    res.extensions_mut().insert(HttpConnect);
                }

                if h1::is_upgrade(&res) {
                    trace!("client response is HTTP/1.1 upgrade");
                } else {
                    h1::strip_connection_headers(res.headers_mut());
                }
                Poll::Ready(Ok(res))
            }
            ClientFuture::Http2(f) => f.poll(cx).map_err(Into::into),
        }
    }
}
