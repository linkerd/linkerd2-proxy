use super::glue::{Body, HyperConnect};
use super::upgrade::{Http11Upgrade, HttpConnect};
use super::{h1, h2, settings::Settings, trace};
use futures::prelude::*;
use http;
use hyper;
use linkerd2_error::Error;
use std::{
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use tower::ServiceExt;
use tracing::{debug, debug_span, trace};
use tracing_futures::Instrument;

/// Configures an HTTP client that uses a `C`-typed connector
#[derive(Debug)]
pub struct MakeClientLayer<B> {
    h2_settings: crate::h2::Settings,
    _marker: PhantomData<fn() -> B>,
}

/// A `MakeService` that can speak either HTTP/1 or HTTP/2.
pub struct MakeClient<C, B> {
    connect: C,
    h2_settings: crate::h2::Settings,
    _marker: PhantomData<fn(B)>,
}

/// The `Service` yielded by `MakeClient::new_service()`.
pub enum Client<C, T, B>
where
    B: hyper::body::HttpBody + 'static,
    C: tower::make::MakeConnection<T> + 'static,
{
    Http1(hyper::Client<HyperConnect<C, T>, B>),
    Http2(h2::Connection<B>),
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

impl<C, B> tower::layer::Layer<C> for MakeClientLayer<B> {
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
    T: AsRef<Settings> + Clone + Send + Sync + 'static,
    B: hyper::body::HttpBody + Send + 'static,
    B::Data: Send,
    B::Error: Into<Error> + Send + Sync,
{
    type Response = Client<C, T, B>;
    type Error = Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        trace!("Building HTTP client");
        let connect = self.connect.clone();
        let h2_settings = self.h2_settings.clone();

        Box::pin(async move {
            match *target.as_ref() {
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
                    .build(HyperConnect::new(
                        connect,
                        target,
                        was_absolute_form,
                    ));
                    Ok(Client::Http1(h1))
                }
                Settings::Http2 => {
                    let h2 = h2::Connect::new(connect, h2_settings)
                        .oneshot(target)
                        .await?;
                    Ok(Client::Http2(h2))
                }
            }
        })
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
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

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
                let is_http_connect = req.method() == &http::Method::CONNECT;
                Box::pin(
                    h1.request(req)
                        .map_ok(move |mut rsp| {
                            if is_http_connect {
                                debug_assert!(upgrade.is_some());
                                rsp.extensions_mut().insert(HttpConnect);
                            }

                            if h1::is_upgrade(&rsp) {
                                trace!("client response is HTTP/1.1 upgrade");
                            } else {
                                h1::strip_connection_headers(rsp.headers_mut());
                            }

                            rsp.map(|b| Body {
                                body: Some(b),
                                upgrade,
                            })
                        })
                        .err_into::<Error>()
                        .instrument(span.clone()),
                )
            }
            Client::Http2(ref mut h2) => Box::pin(
                h2.call(req)
                    .map_ok(|rsp| {
                        rsp.map(|b| Body {
                            body: Some(b),
                            upgrade: None,
                        })
                    })
                    .err_into::<Error>()
                    .instrument(span.clone()),
            ),
        }
    }
}
