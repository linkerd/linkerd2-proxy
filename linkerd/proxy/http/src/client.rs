use crate::{glue::Body, h1, h2, orig_proto};
use futures::prelude::*;
use linkerd2_error::Error;
use linkerd2_stack::layer;
use std::{
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use tower::ServiceExt;
use tracing::{debug, debug_span};
use tracing_futures::{Instrument, Instrumented};

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Settings {
    UnmeshedHttp1,
    MeshedHttp1,
    H2,
}

/// A `MakeService` that can speak either HTTP/1 or HTTP/2.
pub struct MakeClient<C, B> {
    connect: C,
    h2_settings: h2::Settings,
    _marker: PhantomData<fn(B)>,
}

/// The `Service` yielded by `MakeClient::new_service()`.
pub enum Client<C, T, B> {
    H2(h2::Connection<B>),
    UnmeshedHttp1(h1::Client<C, T, B>),
    MeshedHttp1(orig_proto::Upgrade<C, T, B>),
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
            let settings = (&target).into();
            debug!(?settings, "Building HTTP client");

            let client = match settings {
                Settings::H2 => {
                    let h2 = h2::Connect::new(connect, h2_settings)
                        .oneshot(target)
                        .await?;
                    Client::H2(h2)
                }
                Settings::UnmeshedHttp1 => Client::UnmeshedHttp1(h1::Client::new(connect, target)),
                Settings::MeshedHttp1 => {
                    let h2 = h2::Connect::new(connect.clone(), h2_settings)
                        .oneshot(target.clone())
                        .await?;
                    let http1 = h1::Client::new(connect, target);
                    Client::MeshedHttp1(orig_proto::Upgrade::new(http1, h2))
                }
            };

            Ok(client)
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
        let res = match self {
            Self::H2(ref mut svc) => futures::ready!(svc.poll_ready(cx)),
            Self::MeshedHttp1(ref mut svc) => futures::ready!(svc.poll_ready(cx)),
            Self::UnmeshedHttp1(_) => Ok(()),
        };

        Poll::Ready(res)
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        let span = match self {
            Self::H2(_) => debug_span!("h2"),
            Self::UnmeshedHttp1(_) => debug_span!("unmeshed http1"),
            Self::MeshedHttp1 { .. } => debug_span!("meshed http1"),
        };
        let _e = span.enter();
        debug!(
            method = %req.method(),
            uri = %req.uri(),
            version = ?req.version(),
        );
        debug!(headers = ?req.headers());

        match self {
            Self::UnmeshedHttp1(ref mut h1) => h1.request(req),
            Self::MeshedHttp1(ref mut svc) => Box::pin(svc.call(req)) as RspFuture,
            Self::H2(ref mut svc) => Box::pin(svc.call(req)) as RspFuture,
        }
        .instrument(span.clone())
    }
}
