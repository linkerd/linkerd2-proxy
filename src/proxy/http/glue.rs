use bytes::IntoBuf;
use futures::{future, Async, Future, Poll};
use futures::future::Either;
use http;
use h2;
use hyper::{self, body::Payload};
use hyper::client::connect as hyper_connect;
use std::{error::Error as StdError, fmt};
use tower_grpc as grpc;

use drain;
use proxy::http::{HasH2Reason, h1, upgrade::Http11Upgrade};
use svc;
use task::{BoxSendFuture, ErasedExecutor, Executor};
use transport::Connect;

/// Provides optional HTTP/1.1 upgrade support on the body.
#[derive(Debug)]
pub struct HttpBody {
    /// In HttpBody::drop, if this was an HTTP upgrade, the body is taken
    /// to be inserted into the Http11Upgrade half.
    pub(super) body: Option<hyper::Body>,
    pub(super) upgrade: Option<Http11Upgrade>
}

#[derive(Debug)]
pub struct GrpcBody<B>(B);

/// Glue for a `tower::Service` to used as a `hyper::server::Service`.
#[derive(Debug)]
pub(in proxy) struct HyperServerSvc<S, E> {
    service: S,
    /// Watch any spawned HTTP/1.1 upgrade tasks.
    upgrade_drain_signal: drain::Watch,
    /// Executor used to spawn HTTP/1.1 upgrade tasks, and TCP proxies
    /// after they succeed.
    upgrade_executor: E,
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

// ===== impl HttpBody =====

impl Payload for HttpBody {
    type Data = hyper::body::Chunk;
    type Error = h2::Error;

    fn is_end_stream(&self) -> bool {
        self
            .body
            .as_ref()
            .expect("only taken in drop")
            .is_end_stream()
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, Self::Error> {
        self
            .body
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
        self
            .body
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
            let on_upgrade = self
                .body
                .take()
                .expect("take only on drop")
                .on_upgrade();
            upgrade.insert_half(on_upgrade);
        }
    }
}

// ===== impl GrpcBody =====

impl<B> GrpcBody<B> {
    pub fn new(inner: B) -> Self {
        GrpcBody(inner)
    }
}

impl<B> Payload for GrpcBody<B>
where
    B: grpc::Body + Send + 'static,
    B::Data: Send + 'static,
    <B::Data as IntoBuf>::Buf: Send + 'static,
{
    type Data = <B::Data as IntoBuf>::Buf;
    type Error = h2::Error;

    fn is_end_stream(&self) -> bool {
        grpc::Body::is_end_stream(&self.0)
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, Self::Error> {
        let data = try_ready!(grpc::Body::poll_data(&mut self.0));
        Ok(data.map(IntoBuf::into_buf).into())
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, Self::Error> {
        grpc::Body::poll_metadata(&mut self.0).map_err(From::from)
    }
}

impl<B> grpc::Body for GrpcBody<B>
where
    B: grpc::Body,
{
    type Data = B::Data;

    fn is_end_stream(&self) -> bool {
        grpc::Body::is_end_stream(&self.0)
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, grpc::Error> {
        grpc::Body::poll_data(&mut self.0)
    }

    fn poll_metadata(&mut self) -> Poll<Option<http::HeaderMap>, grpc::Error> {
        grpc::Body::poll_metadata(&mut self.0)
    }
}

// ===== impl HyperServerSvc =====

impl<S, E> HyperServerSvc<S, E> {
    pub(in proxy) fn new(
        service: S,
        upgrade_drain_signal: drain::Watch,
        upgrade_executor: E,
    ) -> Self {
        HyperServerSvc {
            service,
            upgrade_drain_signal,
            upgrade_executor,
        }
    }
}

impl<S, E, B> hyper::service::Service for HyperServerSvc<S, E>
where
    S: svc::Service<
        http::Request<HttpBody>,
        Response=http::Response<B>,
    >,
    S::Error: StdError + Send + Sync + 'static,
    B: Payload + Default + Send + 'static,
    E: Executor<BoxSendFuture> + Clone + Send + Sync + 'static,
{
    type ReqBody = hyper::Body;
    type ResBody = B;
    type Error = S::Error;
    type Future = Either<
        S::Future,
        future::FutureResult<http::Response<Self::ResBody>, Self::Error>,
    >;

    fn call(&mut self, mut req: http::Request<Self::ReqBody>) -> Self::Future {
        // Should this rejection happen later in the Service stack?
        //
        // Rejecting here means telemetry doesn't record anything about it...
        //
        // At the same time, this stuff is specifically HTTP1, so it feels
        // proper to not have the HTTP2 requests going through it...
        if h1::is_bad_request(&req) {
            let mut res = http::Response::default();
            *res.status_mut() = http::StatusCode::BAD_REQUEST;
            return Either::B(future::ok(res));
        }

        let upgrade = if h1::wants_upgrade(&req) {
            trace!("server request wants HTTP/1.1 upgrade");
            // Upgrade requests include several "connection" headers that
            // cannot be removed.

            // Setup HTTP Upgrade machinery.
            let halves = Http11Upgrade::new(
                self.upgrade_drain_signal.clone(),
                ErasedExecutor::erase(self.upgrade_executor.clone()),
            );
            req.extensions_mut().insert(halves.client);

            Some(halves.server)
        } else {
            h1::strip_connection_headers(req.headers_mut());
            None
        };


        let req = req.map(move |b| HttpBody {
            body: Some(b),
            upgrade,
        });
        Either::A(self.service.call(req))
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
    C::Connected: Send + 'static,
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
    F::Error: StdError + Send + Sync,
{
    type Item = (F::Item, hyper_connect::Connected);
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let transport = try_ready!(self.inner.poll());
        let connected = hyper_connect::Connected::new()
            .proxy(self.absolute_form);
        Ok(Async::Ready((transport, connected)))
    }
}

// === impl Error ===

impl HasH2Reason for Error {
    fn h2_reason(&self) -> Option<h2::Reason> {
        self
            .0
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
    fn cause(&self) -> Option<&StdError> {
        self.0.cause()
    }
}

