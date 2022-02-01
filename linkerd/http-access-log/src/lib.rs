#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

use futures_core::TryFuture;
use linkerd_identity as identity;
use linkerd_proxy_transport::{ClientAddr, Remote};
use linkerd_stack as svc;
use linkerd_tls as tls;
use linkerd_tracing::access_log::TRACE_TARGET;
use pin_project::pin_project;
use std::{
    future::Future,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
    time::{Duration, Instant, SystemTime},
};
use svc::{NewService, Param};
use tracing::{field, span, Level, Span};

#[derive(Clone, Debug)]
pub struct NewAccessLog<N> {
    inner: N,
}

#[derive(Clone, Debug)]
pub struct AccessLogContext<S> {
    inner: S,
    client_addr: SocketAddr,
    client_id: Option<identity::Name>,
}

struct ResponseFutureInner {
    span: Span,
    start: Instant,
    processing: Duration,
}

#[pin_project]
pub struct AccessLogFuture<F> {
    data: Option<ResponseFutureInner>,

    #[pin]
    inner: F,
}

impl<N> NewAccessLog<N> {
    /// Returns a new `NewAccessLog` layer that wraps an inner service with
    /// access logging middleware.
    ///
    /// The access log is recorded by adding a `tracing` span to the service's
    /// future. If access logging is not enabled by the `tracing` subscriber,
    /// this span will never be enabled, and it can be skipped cheaply. When
    /// access logging *is* enabled, additional data will be recorded when the
    /// response future completes.
    ///
    /// Recording the access log will introduce additional overhead in the
    /// request path, but this is largely avoided when access logging is not
    /// enabled.
    #[inline]
    pub fn layer() -> impl svc::layer::Layer<N, Service = Self> {
        svc::layer::mk(|inner| NewAccessLog { inner })
    }
}

impl<N, T> NewService<T> for NewAccessLog<N>
where
    T: Param<tls::ConditionalServerTls> + Param<Remote<ClientAddr>>,
    N: NewService<T>,
{
    type Service = AccessLogContext<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let Remote(ClientAddr(client_addr)) = target.param();
        let tls: tls::ConditionalServerTls = target.param();
        let client_id = tls
            .value()
            .and_then(|tls| tls.client_id().map(|tls::ClientId(name)| name.clone()));
        let inner = self.inner.new_service(target);
        AccessLogContext {
            inner,
            client_addr,
            client_id,
        }
    }
}

impl<S, B1, B2> svc::Service<http::Request<B1>> for AccessLogContext<S>
where
    S: svc::Service<http::Request<B1>, Response = http::Response<B2>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = AccessLogFuture<S::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<B1>) -> Self::Future {
        let get_header = |name: http::header::HeaderName| {
            request
                .headers()
                .get(name)
                .and_then(|x| x.to_str().ok())
                .unwrap_or_default()
        };

        let trace_id = || {
            let headers = request.headers();
            headers
                .get("x-b3-traceid")
                .or_else(|| headers.get("x-request-id"))
                .or_else(|| headers.get("x-amzn-trace-id"))
                .and_then(|x| x.to_str().ok())
                .unwrap_or_default()
        };

        let span = span!(target: TRACE_TARGET, Level::INFO, "http",
            client.addr = %self.client_addr,
            client.id = self.client_id.as_ref().map(|n| n.as_str()).unwrap_or("-"),
            timestamp = %now(),
            method = request.method().as_str(),
            uri =  %request.uri(),
            version = ?request.version(),
            trace_id = trace_id(),
            request_bytes = get_header(http::header::CONTENT_LENGTH),
            status = field::Empty,
            response_bytes = field::Empty,
            total_ns = field::Empty,
            processing_ns = field::Empty,
            user_agent = get_header(http::header::USER_AGENT),
            host = get_header(http::header::HOST),
        );

        // The access log span is only enabled by the `tracing` subscriber if
        // access logs are being recorded. If it's disabled, we can skip
        // recording additional data in the response future.
        if span.is_disabled() {
            return AccessLogFuture {
                data: None,
                inner: self.inner.call(request),
            };
        }

        AccessLogFuture {
            data: Some(ResponseFutureInner {
                span,
                start: Instant::now(),
                processing: Duration::from_secs(0),
            }),
            inner: self.inner.call(request),
        }
    }
}

impl<F, B2> Future for AccessLogFuture<F>
where
    F: TryFuture<Ok = http::Response<B2>>,
{
    type Output = Result<F::Ok, F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        let data: &mut ResponseFutureInner = match &mut this.data {
            Some(data) => data,
            None => return this.inner.try_poll(cx),
        };

        let _enter = data.span.enter();
        let poll_start = Instant::now();

        let response: http::Response<B2> = match this.inner.try_poll(cx) {
            Poll::Pending => {
                data.processing += Instant::now().saturating_duration_since(poll_start);
                return Poll::Pending;
            }
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Ready(Ok(response)) => response,
        };

        let now = Instant::now();
        let total_ns = now.duration_since(data.start).as_nanos();
        let processing_ns = (now.duration_since(poll_start) + data.processing).as_nanos();

        let span = &data.span;

        response
            .headers()
            .get(http::header::CONTENT_LENGTH)
            .and_then(|x| x.to_str().ok())
            .map(|x| span.record("response_bytes", &x));

        span.record("status", &response.status().as_u16());
        span.record("total_ns", &field::display(total_ns));
        span.record("processing_ns", &field::display(processing_ns));

        Poll::Ready(Ok(response))
    }
}

#[inline]
fn now() -> humantime::Rfc3339Timestamp {
    humantime::format_rfc3339(SystemTime::now())
}
