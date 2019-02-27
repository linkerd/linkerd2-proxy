use bytes::Bytes;
use futures::{Async, Future, Poll};
use h2;
use http;
use hyper::client::connect as hyper_connect;
use hyper::{self, body::Payload};
use std::{error::Error as StdError, fmt};
use tower_grpc as grpc;

use proxy::http::{upgrade::Http11Upgrade, HasH2Reason};
use svc;
use transport::{tls::HasStatus as HasTlsStatus, Connect};
use Conditional;

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
pub struct HyperConnect<C> {
    connect: C,
    absolute_form: bool,
}

/// Future returned by `HyperConnect`.
pub struct HyperConnectFuture<F> {
    inner: F,
    absolute_form: bool,
}

/// Wrapper of hyper::Error so we can add methods.
pub struct Error(hyper::Error);

/// Marker in `Response` extensions if the connection used TLS.
#[derive(Clone, Debug)]
pub struct ClientUsedTls(pub(super) ());

// ===== impl HttpBody =====

impl Payload for HttpBody {
    type Data = hyper::body::Chunk;
    type Error = h2::Error;

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
                Error(e)
                    .h2_reason()
                    .unwrap_or(h2::Reason::INTERNAL_ERROR)
                    .into()
            })
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, Self::Error> {
        self.body
            .as_mut()
            .expect("only taken in drop")
            .poll_trailers()
            .map_err(|e| {
                debug!("http trailers error: {}", e);
                Error(e)
                    .h2_reason()
                    .unwrap_or(h2::Reason::INTERNAL_ERROR)
                    .into()
            })
    }
}

impl grpc::Body for HttpBody {
    type Data = Bytes;

    fn is_end_stream(&self) -> bool {
        Payload::is_end_stream(self)
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, grpc::Error> {
        match Payload::poll_data(self) {
            Ok(Async::Ready(Some(chunk))) => Ok(Async::Ready(Some(chunk.into()))),
            Ok(Async::Ready(None)) => Ok(Async::Ready(None)),
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(err) => Err(err.into()),
        }
    }

    fn poll_metadata(&mut self) -> Poll<Option<http::HeaderMap>, grpc::Error> {
        Payload::poll_trailers(self).map_err(From::from)
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

impl super::retry::TryClone for HttpBody {
    fn try_clone(&self) -> Option<Self> {
        if self.is_end_stream() {
            Some(HttpBody::default())
        } else {
            None
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
    S: svc::Service<http::Request<HttpBody>, Response = http::Response<B>>,
    S::Error: StdError + Send + Sync + 'static,
    B: Payload,
{
    type ReqBody = hyper::Body;
    type ResBody = B;
    type Error = S::Error;
    type Future = S::Future;

    fn call(&mut self, req: http::Request<Self::ReqBody>) -> Self::Future {
        self.service.call(req.map(|b| HttpBody {
            body: Some(b),
            upgrade: None,
        }))
    }
}

// ===== impl HyperConnect =====

impl<C> HyperConnect<C>
where
    C: Connect,
    C::Future: 'static,
{
    pub(in proxy) fn new(connect: C, absolute_form: bool) -> Self {
        HyperConnect {
            connect,
            absolute_form,
        }
    }
}

impl<C> hyper_connect::Connect for HyperConnect<C>
where
    C: Connect + Send + Sync,
    C::Future: Send + 'static,
    <C::Future as Future>::Error: StdError + Send + Sync + 'static,
    C::Connected: HasTlsStatus + Send + 'static,
{
    type Transport = C::Connected;
    type Error = <C::Future as Future>::Error;
    type Future = HyperConnectFuture<C::Future>;

    fn connect(&self, _dst: hyper_connect::Destination) -> Self::Future {
        HyperConnectFuture {
            inner: self.connect.connect(),
            absolute_form: self.absolute_form,
        }
    }
}

impl<F> Future for HyperConnectFuture<F>
where
    F: Future + 'static,
    F::Item: HasTlsStatus,
    F::Error: StdError + Send + Sync,
{
    type Item = (F::Item, hyper_connect::Connected);
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let transport = try_ready!(self.inner.poll());
        let connected = hyper_connect::Connected::new().proxy(self.absolute_form);
        let connected = if let Conditional::Some(()) = transport.tls_status() {
            connected.extra(ClientUsedTls(()))
        } else {
            connected
        };
        Ok(Async::Ready((transport, connected)))
    }
}

// === impl Error ===

impl HasH2Reason for Error {
    fn h2_reason(&self) -> Option<h2::Reason> {
        self.0
            .cause2()
            .and_then(|cause| cause.downcast_ref::<h2::Error>())
            .and_then(|err| err.reason())
    }
}

impl From<hyper::Error> for Error {
    fn from(err: hyper::Error) -> Error {
        Error(err)
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(StdError + 'static)> {
        self.0.source()
    }
}
