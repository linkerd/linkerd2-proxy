use super::glue::{Body, HyperConnect};
use super::upgrade::{Http11Upgrade, HttpConnect};
use super::{
    h1, h2,
    settings::{HasSettings, Settings},
    trace,
};
use futures::{ready, TryFuture};
use http;
use hyper;
use linkerd2_error::Error;
use pin_project::pin_project;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};
use tower::ServiceExt;
use tracing::{debug, debug_span, trace};
use tracing_futures::{Instrument, Instrumented};

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
#[pin_project(project = MakeFutureProj)]
pub enum MakeFuture<C, T, B>
where
    B: hyper::body::HttpBody + Send + 'static,
    B::Data: Send,
    B::Error: Into<Error> + Send + Sync,
    C: tower::make::MakeConnection<T> + 'static,
    C::Error: Into<Error>,
    C::Connection: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    C::Future: Send + 'static,
{
    Http1(Option<HyperMakeClient<C, T, B>>),
    Http2(#[pin] tower::util::Oneshot<h2::Connect<C, B>, T>),
}

/// The `Service` yielded by `MakeClient::new_service()`.
pub enum Client<C, T, B>
where
    B: hyper::body::HttpBody + 'static,
    C: tower::make::MakeConnection<T> + 'static,
{
    Http1(HyperMakeClient<C, T, B>),
    Http2(h2::Connection<B>),
}

#[pin_project(project = ClientFutureProj)]
pub enum ClientFuture {
    Http1 {
        #[pin]
        future: Instrumented<hyper::client::ResponseFuture>,
        upgrade: Option<Http11Upgrade>,
        is_http_connect: bool,
    },
    Http2(#[pin] Instrumented<h2::ResponseFuture>),
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

impl<B> Clone for MakeClientLayer<B> {
    fn clone(&self) -> Self {
        Self {
            h2_settings: self.h2_settings,
            _marker: self._marker,
        }
    }
}

impl<C, B> tower::layer::Layer<C> for MakeClientLayer<B>
// where
// B: hyper::body::HttpBody + Send + 'static,
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

impl<C, T, B> tower::Service<T> for MakeClient<C, B>
where
    C: tower::make::MakeConnection<T> + Clone + Unpin + Send + Sync + 'static,
    C::Future: Unpin + Send + 'static,
    C::Error: Into<Error>,
    C::Connection: Unpin + Send + 'static,
    T: HasSettings + Clone + Send + Sync + 'static,
    B: hyper::body::HttpBody + Send + 'static,
    B::Data: Send,
    B::Error: Into<Error> + Send + Sync,
{
    type Response = Client<C, T, B>;
    type Error = Error;
    type Future = MakeFuture<C, T, B>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.connect.poll_ready(cx).map_err(Into::into)
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
                let mut h1 = hyper::Client::builder();
                let h1 = if !keep_alive {
                    // disable hyper's connection pooling by setting the maximum
                    // number of idle connections to 0.
                    h1.pool_max_idle_per_host(0)
                } else {
                    &mut h1
                }
                // hyper should only try to automatically
                // set the host if the request was in absolute_form
                .executor(trace::Executor::new())
                .set_host(was_absolute_form)
                .build(HyperConnect::new(connect, target, was_absolute_form));
                MakeFuture::Http1(Some(h1))
            }
            Settings::Http2 => {
                let h2 = h2::Connect::new(connect, self.h2_settings.clone()).oneshot(target);
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
    C: tower::make::MakeConnection<T> + Unpin + Send + Sync + 'static,
    C::Connection: Unpin + Send + 'static,
    C::Future: Send + 'static,
    C::Error: Into<Error>,
    B: hyper::body::HttpBody + Send + 'static,
    B::Data: Send,
    B::Error: Into<Error> + Send + Sync,
{
    type Output = Result<Client<C, T, B>, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let svc = match self.project() {
            MakeFutureProj::Http1(h1) => Client::Http1(h1.take().expect("polled after ready")),
            MakeFutureProj::Http2(h2) => {
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
    C: tower::make::MakeConnection<T> + Clone + Send + Sync + 'static,
    C::Connection: Unpin + Send + 'static,
    C::Future: Unpin + Send + 'static,
    C::Error: Into<Error>,
    T: Clone + Send + Sync + 'static,
    B: hyper::body::HttpBody + Send + 'static,
    B::Data: Send,
    B::Error: Into<Error> + Send + Sync,
{
    type Response = http::Response<Body>;
    type Error = Error;
    type Future = ClientFuture;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match *self {
            Client::Http1(_) => Poll::Ready(Ok(())),
            Client::Http2(ref mut h2) => h2.poll_ready(cx).map_err(Into::into),
        }
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        let span = debug_span!(
            "request",
            method = %req.method(),
            uri = %req.uri(),
            version = ?req.version(),
        );
        let _e = span.enter();
        debug!(headers = ?req.headers(), "client request");

        match *self {
            Client::Http1(ref h1) => {
                let upgrade = req.extensions_mut().remove::<Http11Upgrade>();
                let is_http_connect = if upgrade.is_some() {
                    req.method() == &http::Method::CONNECT
                } else {
                    false
                };
                ClientFuture::Http1 {
                    future: h1.request(req).instrument(span.clone()),
                    upgrade,
                    is_http_connect,
                }
            }
            Client::Http2(ref mut h2) => ClientFuture::Http2(h2.call(req).instrument(span.clone())),
        }
    }
}

// === impl ClientFuture ===

impl Future for ClientFuture {
    type Output = Result<http::Response<Body>, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            ClientFutureProj::Http1 {
                future,
                upgrade,
                is_http_connect,
            } => {
                let mut res = ready!(future.try_poll(cx))?.map(|b| Body {
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
                Poll::Ready(Ok(res))
            }
            ClientFutureProj::Http2(f) => f.poll(cx).map_err(Into::into),
        }
    }
}
