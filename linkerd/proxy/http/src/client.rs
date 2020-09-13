use crate::{glue::Body, h1, h2, Settings};
use futures::prelude::*;
use linkerd2_error::Error;
use linkerd2_stack::layer;
use std::{
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use tower::ServiceExt;
use tracing::{debug, debug_span, info, trace};
use tracing_futures::{Instrument, Instrumented};

/// A `MakeService` that can speak either HTTP/1 or HTTP/2.
pub struct MakeClient<C, B> {
    connect: C,
    h2_settings: crate::h2::Settings,
    _marker: PhantomData<fn(B)>,
}

/// The `Service` yielded by `MakeClient::new_service()`.
pub struct Client<C, T, B> {
    h1: h1::Client<C, T, B>,
    h2: Option<h2::Connection<B>>,
}

pub fn layer<C, B>(
    h2_settings: h2::Settings,
) -> impl layer::Layer<C, Service = MakeClient<C, B>> + Copy {
    layer::mk(move |connect: C| MakeClient {
        connect,
        h2_settings,
        _marker: PhantomData,
    })
}

// === impl MakeClient ===

impl<C, T, B> tower::Service<T> for MakeClient<C, B>
where
    T: Clone + Send + Sync + 'static,
    for<'t> &'t T: Into<Settings>,
    C: tower::make::MakeConnection<T> + Clone + Unpin + Send + Sync + 'static,
    C::Future: Unpin + Send + 'static,
    C::Error: Into<Error>,
    C::Connection: Unpin + Send + 'static,
    B: hyper::body::HttpBody + Send + 'static,
    B::Data: Send,
    B::Error: Into<Error> + Send + Sync,
{
    type Response = Client<C, T, B>;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Client<C, T, B>, Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        let connect = self.connect.clone();
        let h2_settings = self.h2_settings;

        Box::pin(async move {
            let h2 = match (&target).into() {
                Settings::Http1 => None,
                Settings::Http2 => {
                    trace!("Building H2 client");
                    let h2 = h2::Connect::new(connect.clone(), h2_settings)
                        .oneshot(target.clone())
                        .await?;
                    Some(h2)
                }
            };

            trace!("Building HTTP client");
            let h1 = h1::Client::new(connect, target);

            Ok(Client { h1, h2 })
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

type RspFuture =
    Pin<Box<dyn Future<Output = Result<http::Response<Body>, hyper::Error>> + Send + 'static>>;

impl<C, T, B> tower::Service<http::Request<B>> for Client<C, T, B>
where
    T: Clone + Send + Sync + 'static,
    C: tower::make::MakeConnection<T> + Clone + Send + Sync + 'static,
    C::Connection: Unpin + Send + 'static,
    C::Future: Unpin + Send + 'static,
    C::Error: Into<Error>,
    B: hyper::body::HttpBody + Send + 'static,
    B::Data: Send,
    B::Error: Into<Error> + Send + Sync,
{
    type Response = http::Response<Body>;
    type Error = hyper::Error;
    type Future = Instrumented<RspFuture>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let res = match self.h2.as_mut() {
            Some(ref mut h2) => futures::ready!(h2.poll_ready(cx)),
            None => Ok(()),
        };

        Poll::Ready(res)
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

        if req.version() == http::Version::HTTP_2 {
            if let Some(ref mut client) = self.h2.as_mut() {
                let fut: RspFuture = Box::pin(client.call(req).map_ok(|rsp| rsp.map(Body::from)));
                return fut.instrument(span.clone());
            }
            debug!("H2 request to endpoint without an H2 client")
        }

        match req.version() {
            http::Version::HTTP_09 | http::Version::HTTP_10 | http::Version::HTTP_11 => {}
            _ => {
                info!("Downgrading version to HTTP/1.1");
                *req.version_mut() = http::Version::HTTP_11
            }
        }

        self.h1.request(req).instrument(span.clone())
    }
}
