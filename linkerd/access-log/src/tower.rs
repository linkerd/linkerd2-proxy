use futures::TryFuture;
use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tracing::{field, span, Level, Span};

/// A tower layer that associates a tokio-tracing Span with each request
#[derive(Clone)]
pub struct AccessLogLayer {}

#[derive(Clone)]
pub struct AccessLogContext<Svc> {
    inner: Svc,
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

impl<Svc> tower::layer::Layer<Svc> for AccessLogLayer {
    type Service = AccessLogContext<Svc>;

    fn layer(&self, inner: Svc) -> Self::Service {
        Self::Service { inner }
    }
}

impl<Svc, B1, B2> tower::Service<http::Request<B1>> for AccessLogContext<Svc>
where
    Svc: tower::Service<http::Request<B1>, Response = http::Response<B2>>,
{
    type Response = Svc::Response;
    type Error = Svc::Error;
    type Future = AccessLogFuture<Svc::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Svc::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<B1>) -> Self::Future {
        let span: Span = span!(target: "access_log", Level::TRACE, "http",
            timestamp=field::Empty, processing_ns=field::Empty, total_ns=field::Empty,
            method=field::Empty, uri=field::Empty, version=field::Empty, user_agent=field::Empty,
            host=field::Empty, trace_id=field::Empty, status=field::Empty,
            request_bytes=field::Empty, response_bytes=field::Empty);

        if span.is_disabled() {
            return AccessLogFuture {
                data: None,
                inner: self.inner.call(request),
            };
        }

        // Delay formatting to avoid an intermediate `String`
        let delayed_format = chrono::Utc::now().format_with_items(
            [chrono::format::Item::Fixed(chrono::format::Fixed::RFC3339)].iter(),
        );

        span.record("timestamp", &field::display(&delayed_format));
        span.record("method", &request.method().as_str());
        span.record("uri", &field::display(&request.uri()));
        span.record("version", &field::debug(&request.version()));

        request
            .headers()
            .get("Host")
            .and_then(|x| x.to_str().ok())
            .map(|x| span.record("host", &x));

        request
            .headers()
            .get("User-Agent")
            .and_then(|x| x.to_str().ok())
            .map(|x| span.record("user_agent", &x));

        request
            .headers()
            .get("Content-Length")
            .and_then(|x| x.to_str().ok())
            .map(|x| span.record("request_bytes", &x));

        request
            .headers()
            .get("x-b3-traceid")
            .or_else(|| request.headers().get("X-Request-ID"))
            .or_else(|| request.headers().get("X-Amzn-Trace-Id"))
            .and_then(|x| x.to_str().ok())
            .map(|x| span.record("trace_id", &x));

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

impl AccessLogLayer {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for AccessLogLayer {
    fn default() -> Self {
        Self::new()
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
                data.processing += Instant::now().duration_since(poll_start);
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
            .get("Content-Length")
            .and_then(|x| x.to_str().ok())
            .map(|x| span.record("response_bytes", &x));

        span.record("status", &response.status().as_u16());
        span.record("total_ns", &field::display(total_ns));
        span.record("processing_ns", &field::display(processing_ns));

        Poll::Ready(Ok(response))
    }
}
