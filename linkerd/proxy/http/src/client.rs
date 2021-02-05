//! The proxy's HTTP client.
//!
//! It can operate as a pure HTTP/1 client, a pure HTTP/2 client, or a
//! mixed-mode "orig-proto" client. The orig-proto mode attempts to dispatch
//! HTTP/1 messages over an H2 transport; however, some requests cannot be
//! proxied via this method, so it also maintains a fallback HTTP/1 client.

use crate::{glue::UpgradeBody, h1, h2, orig_proto};
use futures::prelude::*;
use linkerd_error::Error;
use linkerd_stack::{layer, Param};
use std::{
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use tower::ServiceExt;
use tracing::instrument::{Instrument, Instrumented};
use tracing::{debug, debug_span};

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Settings {
    Http1,
    H2,
    OrigProtoUpgrade,
}

pub struct MakeClient<C, B> {
    connect: C,
    h1_pool: h1::PoolSettings,
    h2_settings: h2::Settings,
    _marker: PhantomData<fn(B)>,
}

pub enum Client<C, T, B> {
    H2(h2::Connection<B>),
    Http1(h1::Client<C, T, B>),
    OrigProtoUpgrade(orig_proto::Upgrade<C, T, B>),
}

pub fn layer<C, B>(
    h1_pool: h1::PoolSettings,
    h2_settings: h2::Settings,
) -> impl layer::Layer<C, Service = MakeClient<C, B>> + Copy {
    layer::mk(move |connect: C| MakeClient {
        connect,
        h1_pool,
        h2_settings,
        _marker: PhantomData,
    })
}

impl From<crate::Version> for Settings {
    fn from(v: crate::Version) -> Self {
        match v {
            crate::Version::Http1 => Self::Http1,
            crate::Version::H2 => Self::H2,
        }
    }
}

// === impl MakeClient ===

type MakeFuture<C, T, B> =
    Pin<Box<dyn Future<Output = Result<Client<C, T, B>, Error>> + Send + 'static>>;

impl<C, T, B> tower::Service<T> for MakeClient<C, B>
where
    T: Clone + Send + Sync + 'static,
    T: Param<Settings>,
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
    type Future = MakeFuture<C, T, B>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        let connect = self.connect.clone();
        let h1_pool = self.h1_pool;
        let h2_settings = self.h2_settings;

        Box::pin(async move {
            let settings = target.param();
            debug!(?settings, "Building HTTP client");

            let client = match settings {
                Settings::H2 => {
                    let h2 = h2::Connect::new(connect, h2_settings)
                        .oneshot(target)
                        .await?;
                    Client::H2(h2)
                }
                Settings::Http1 => Client::Http1(h1::Client::new(connect, target, h1_pool)),
                Settings::OrigProtoUpgrade => {
                    let h2 = h2::Connect::new(connect.clone(), h2_settings)
                        .oneshot(target.clone())
                        .await?;
                    let http1 = h1::Client::new(connect, target, h1_pool);
                    Client::OrigProtoUpgrade(orig_proto::Upgrade::new(http1, h2))
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
            h1_pool: self.h1_pool,
            h2_settings: self.h2_settings,
            _marker: self._marker,
        }
    }
}

// === impl Client ===

type RspFuture = Pin<
    Box<dyn Future<Output = Result<http::Response<UpgradeBody>, hyper::Error>> + Send + 'static>,
>;

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
    type Response = http::Response<UpgradeBody>;
    type Error = hyper::Error;
    type Future = Instrumented<RspFuture>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let res = match self {
            Self::H2(ref mut svc) => futures::ready!(svc.poll_ready(cx)),
            Self::OrigProtoUpgrade(ref mut svc) => futures::ready!(svc.poll_ready(cx)),
            Self::Http1(_) => Ok(()),
        };

        Poll::Ready(res)
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
                Self::H2(ref mut svc) => {
                    Box::pin(svc.call(req).map_ok(|rsp| rsp.map(UpgradeBody::from))) as RspFuture
                }
            }
        })
        .instrument(span)
    }
}
