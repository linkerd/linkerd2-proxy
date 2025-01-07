//! The proxy's HTTP client.
//!
//! It can operate as a pure HTTP/1 client, a pure HTTP/2 client, or a
//! mixed-mode "orig-proto" client. The orig-proto mode attempts to dispatch
//! HTTP/1 messages over an H2 transport; however, some requests cannot be
//! proxied via this method, so it also maintains a fallback HTTP/1 client.

use crate::{h1, h2, orig_proto};
use futures::prelude::*;
use linkerd_error::{Error, Result};
use linkerd_http_box::BoxBody;
use linkerd_stack::{layer, ExtractParam, MakeConnection, Service, ServiceExt};
use std::{
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use tracing::instrument::{Instrument, Instrumented};
use tracing::{debug, debug_span};

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum Params {
    Http1(h1::PoolSettings),
    H2(h2::ClientParams),
    OrigProtoUpgrade(h2::ClientParams, h1::PoolSettings),
}

pub struct MakeClient<X, C, B> {
    connect: C,
    params: X,
    _marker: PhantomData<fn(B)>,
}

pub enum Client<C, T, B> {
    H2(h2::Connection<B>),
    Http1(h1::Client<C, T, B>),
    OrigProtoUpgrade(orig_proto::Upgrade<C, T, B>),
}

pub fn layer_via<X: Clone, C, B>(
    params: X,
) -> impl layer::Layer<C, Service = MakeClient<X, C, B>> + Clone {
    layer::mk(move |connect: C| MakeClient {
        connect,
        params: params.clone(),
        _marker: PhantomData,
    })
}

pub fn layer<C, B>() -> impl layer::Layer<C, Service = MakeClient<(), C, B>> + Clone {
    layer_via(())
}

// === impl MakeClient ===

type MakeFuture<C, T, B> = Pin<Box<dyn Future<Output = Result<Client<C, T, B>>> + Send + 'static>>;

impl<X, C, T, B> tower::Service<T> for MakeClient<X, C, B>
where
    T: Clone + Send + Sync + 'static,
    X: ExtractParam<Params, T>,
    C: MakeConnection<(crate::Version, T)> + Clone + Unpin + Send + Sync + 'static,
    C::Connection: hyper::rt::Read + hyper::rt::Write + Send + Unpin,
    C::Metadata: Send,
    C::Future: Unpin + Send + 'static,
    B: crate::Body + Unpin + Send + 'static,
    B::Data: Send,
    B::Error: Into<Error> + Send + Sync,
{
    type Response = Client<C, T, B>;
    type Error = Error;
    type Future = MakeFuture<C, T, B>;

    #[inline]
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        let connect = self.connect.clone();
        let settings = self.params.extract_param(&target);

        Box::pin(async move {
            debug!(?settings, "Building HTTP client");
            let client = match settings {
                Params::H2(params) => {
                    let h2 = h2::Connect::new(connect, params).oneshot(target).await?;
                    Client::H2(h2)
                }
                Params::Http1(params) => Client::Http1(h1::Client::new(connect, target, params)),
                Params::OrigProtoUpgrade(h2params, h1params) => {
                    let h2 = h2::Connect::new(connect.clone(), h2params)
                        .oneshot(target.clone())
                        .await?;
                    let http1 = h1::Client::new(connect, target, h1params);
                    Client::OrigProtoUpgrade(orig_proto::Upgrade::new(http1, h2))
                }
            };

            Ok(client)
        })
    }
}

impl<X: Clone, C: Clone, B> Clone for MakeClient<X, C, B> {
    fn clone(&self) -> Self {
        Self {
            connect: self.connect.clone(),
            params: self.params.clone(),
            _marker: self._marker,
        }
    }
}

// === impl Client ===

type RspFuture = Pin<Box<dyn Future<Output = Result<http::Response<BoxBody>>> + Send + 'static>>;

impl<C, T, B> Service<http::Request<B>> for Client<C, T, B>
where
    T: Clone + Send + Sync + 'static,
    C: MakeConnection<(crate::Version, T)> + Clone + Send + Sync + 'static,
    C::Connection: hyper::rt::Read + hyper::rt::Write + Unpin + Send,
    C::Future: Unpin + Send + 'static,
    C::Error: Into<Error>,
    B: crate::Body + Unpin + Send + 'static,
    B::Data: Send,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    type Response = http::Response<BoxBody>;
    type Error = Error;
    type Future = Instrumented<RspFuture>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<()>> {
        match self {
            Self::H2(ref mut svc) => svc.poll_ready(cx).map_err(Into::into),
            Self::OrigProtoUpgrade(ref mut svc) => svc.poll_ready(cx),
            Self::Http1(_) => Poll::Ready(Ok(())),
        }
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        let span = match self {
            Self::H2(_) => debug_span!("h2"),
            Self::Http1(_) => debug_span!("http1"),
            Self::OrigProtoUpgrade { .. } => debug_span!("orig-proto-upgrade"),
        };
        span.in_scope(|| {
            debug!(
                method = %req.method(),
                uri = %req.uri(),
                version = ?req.version(),
            );
            debug!(headers = ?req.headers());

            match self {
                Self::Http1(ref mut h1) => h1.request(req),
                Self::OrigProtoUpgrade(ref mut svc) => svc.call(req),
                Self::H2(ref mut svc) => Box::pin(
                    svc.call(req)
                        .err_into::<Error>()
                        .map_ok(|rsp| rsp.map(BoxBody::new)),
                ) as RspFuture,
            }
        })
        .instrument(span.or_current())
    }
}
