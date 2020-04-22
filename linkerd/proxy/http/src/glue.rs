use crate::{upgrade::Http11Upgrade, HasH2Reason};
use futures::{try_ready, Async, Future, Poll};
use futures_03::{compat::Compat, TryFutureExt};
use http;
use hyper::client::connect as hyper_connect;
use hyper::{self, body::Payload};
use linkerd2_error::Error;
use std::pin::Pin;
use tracing::debug;

/// Provides optional HTTP/1.1 upgrade support on the body.
#[derive(Debug)]
pub struct HttpBody {
    /// In HttpBody::drop, if this was an HTTP upgrade, the body is taken
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

// ===== impl HttpBody =====

impl Payload for HttpBody {
    type Data = hyper::body::Chunk;
    type Error = hyper::Error;

    fn is_end_stream(&self) -> bool {
        self.body
            .as_ref()
            .expect("only taken in drop")
            .is_end_stream()
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, Self::Error> {
        self.body
            .as_mut()
            .expect("only taken in drop")
            .poll_data()
            .map_err(|e| {
                debug!("http body error: {}", e);
                e
            })
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, Self::Error> {
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

impl http_body::Body for HttpBody {
    type Data = hyper::body::Chunk;
    type Error = hyper::Error;

    fn is_end_stream(&self) -> bool {
        Payload::is_end_stream(self)
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, Self::Error> {
        Payload::poll_data(self)
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, Self::Error> {
        Payload::poll_trailers(self)
    }
}

impl Default for HttpBody {
    fn default() -> HttpBody {
        HttpBody {
            body: Some(hyper::Body::empty()),
            upgrade: None,
        }
    }
}

impl Drop for HttpBody {
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

impl<S, B> hyper::service::Service for HyperServerSvc<S>
where
    S: tower_03::Service<http::Request<HttpBody>, Response = http::Response<B>>,
    S::Error: Into<Error>,
    B: Payload,
{
    type ReqBody = hyper::Body;
    type ResBody = B;
    type Error = S::Error;
    type Future = Compat<Pin<Box<S::Future>>>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        task_compat::poll_03_to_01(task_compat::with_context(|cx| self.service.poll_ready(cx)))
    }

    fn call(&mut self, req: http::Request<Self::ReqBody>) -> Self::Future {
        Box::pin(self.service.call(req.map(|b| HttpBody {
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
    C::Future: Send + 'static,
    <C::Future as Future>::Error: Into<Error>,
    C::Connection: Send + 'static,
    T: Clone + Send + Sync,
{
    type Transport = C::Connection;
    type Error = <C::Future as Future>::Error;
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
    F: Future + 'static,
    F::Error: Into<Error>,
{
    type Item = (F::Item, hyper_connect::Connected);
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
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
