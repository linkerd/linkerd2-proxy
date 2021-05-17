use futures::TryFuture;
use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tracing::{field, span, Level, Span};

/// A tower layer that associates a `tracing` `Span` with each request
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

impl<S, B1, B2> tower::Service<http::Request<B1>> for AccessLogContext<S>
where
    S: tower::Service<http::Request<B1>, Response = http::Response<B2>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = AccessLogFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<B1>) -> Self::Future {
        let get_header = |name: &str| {
            request
                .headers()
                .get(name)
                .and_then(|x| x.to_str().ok())
                .unwrap_or_default()
        };

        let trace_id = || {
            request
                .headers()
                .get("x-b3-traceid")
                .or_else(|| request.headers().get("X-Request-ID"))
                .or_else(|| request.headers().get("X-Amzn-Trace-Id"))
                .and_then(|x| x.to_str().ok())
                .unwrap_or_default()
        };

        let timestamp = chrono::Utc::now().format_with_items(
            [chrono::format::Item::Fixed(chrono::format::Fixed::RFC3339)].iter(),
        );

        let span = span!(target: "access_log", Level::TRACE, "http",
            %timestamp,
            processing_ns=field::Empty,
            total_ns=field::Empty,
            method=&request.method().as_str(),
            uri=&field::display(&request.uri()),
            version=&field::debug(&request.version()),
            user_agent=get_header("User-Agent"),
            host=get_header("Host"),
            trace_id=trace_id(),
            status=field::Empty,
            request_bytes=get_header("Content-Length"),
            response_bytes=field::Empty
        );

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
