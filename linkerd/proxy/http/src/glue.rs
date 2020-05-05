use crate::{upgrade::Http11Upgrade, HasH2Reason};
use bytes::Bytes;
use futures_03::{TryFuture, TryFutureExt};
use http;
use hyper::client::connect as hyper_connect;
use hyper::{self, body::HttpBody};
use linkerd2_error::Error;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tracing::debug;

/// Provides optional HTTP/1.1 upgrade support on the body.
#[derive(Debug)]
pub struct UpgradeBody {
    /// In UpgradeBody::drop, if this was an HTTP upgrade, the body is taken
    /// to be inserted into the Http11Upgrade half.
    pub(super) body: Option<hyper::Body>,
    pub(super) upgrade: Option<Http11Upgrade>,
}

/// Glue for a `tower::Service` to used as a `hyper::server::Service`.
#[derive(Debug)]
pub struct HyperServerSvc<S> {
    service: S,
}

/// Glue for any `tokio_connect::Connect` to implement `hyper::client::Connect`.
#[derive(Debug, Clone)]
pub struct HyperConnect<C, T> {
    connect: C,
    absolute_form: bool,
    target: T,
}

/// Future returned by `HyperConnect`.
pub struct HyperConnectFuture<F> {
    inner: F,
    absolute_form: bool,
}

// ===== impl UpgradeBody =====

impl HttpBody for UpgradeBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn is_end_stream(&self) -> bool {
        self.body
            .as_ref()
            .expect("only taken in drop")
            .is_end_stream()
    }

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        self.body
            .as_mut()
            .expect("only taken in drop")
            .poll_data()
            .map_err(|e| {
                debug!("http body error: {}", e);
                e
            })
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        self.body
            .as_mut()
            .expect("only taken in drop")
            .poll_trailers()
            .map_err(|e| {
                debug!("http trailers error: {}", e);
                e
            })
    }
}

impl http_body::Body for UpgradeBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn is_end_stream(&self) -> bool {
        HttpBody::is_end_stream(self)
    }

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        HttpBody::poll_data(self)
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        HttpBody::poll_trailers(self)
    }
}

impl Default for UpgradeBody {
    fn default() -> UpgradeBody {
        UpgradeBody {
            body: Some(hyper::Body::empty()),
            upgrade: None,
        }
    }
}

impl Drop for UpgradeBody {
    fn drop(&mut self) {
        // If an HTTP/1 upgrade was wanted, send the upgrade future.
        if let Some(upgrade) = self.upgrade.take() {
            let on_upgrade = self.body.take().expect("take only on drop").on_upgrade();
            upgrade.insert_half(on_upgrade);
        }
    }
}

// ===== impl HyperServerSvc =====

impl<S> HyperServerSvc<S> {
    pub fn new(service: S) -> Self {
        HyperServerSvc { service }
    }
}

impl<S, B> tower_03::Service<http::Request<B>> for HyperServerSvc<S>
where
    S: tower_03::Service<http::Request<UpgradeBody>, Response = http::Response<B>>,
    S::Error: Into<Error>,
    B: HttpBody,
{
    type Response = http::Response<B>;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {}

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        Box::pin(self.service.call(req.map(|b| UpgradeBody {
            body: Some(b),
            upgrade: None,
        })))
        .compat()
    }
}

// ===== impl HyperConnect =====

impl<C, T> HyperConnect<C, T> {
    pub(super) fn new(connect: C, target: T, absolute_form: bool) -> Self {
        HyperConnect {
            connect,
            absolute_form,
            target,
        }
    }
}

impl<C, T> hyper_connect::Connect for HyperConnect<C, T>
where
    C: tower::MakeConnection<T> + Clone + Send + Sync,
    C::Future: TryFuture + Send + 'static,
    C::Error: Into<Error>,
    C::Connection: Send + 'static,
    T: Clone + Send + Sync,
{
    type Transport = C::Connection;
    type Error = C::Error;
    type Future = HyperConnectFuture<C::Future>;

    fn connect(&self, _dst: hyper_connect::Destination) -> Self::Future {
        HyperConnectFuture {
            inner: self.connect.clone().make_connection(self.target.clone()),
            absolute_form: self.absolute_form,
        }
    }
}

impl<F> Future for HyperConnectFuture<F>
where
    F: TryFuture + 'static,
    F::Error: Into<Error>,
{
    type Output = Result<(F::Ok, hyper_connect::Connected), F::Error>;

    fn poll(&mut self) -> Poll<Resylt<Self::Item, Self::Error>> {
        let transport = try_ready!(self.inner.poll());
        let connected = hyper_connect::Connected::new().proxy(self.absolute_form);
        Ok(Async::Ready((transport, connected)))
    }
}

// === impl Error ===

impl HasH2Reason for hyper::Error {
    fn h2_reason(&self) -> Option<h2::Reason> {
        (self as &(dyn std::error::Error + 'static)).h2_reason()
    }
}
