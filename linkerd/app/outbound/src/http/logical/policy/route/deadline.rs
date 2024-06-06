use futures::FutureExt;
use linkerd_app_core::{proxy::http, svc, Error, Result};
use linkerd_proxy_client_policy::http::Timeouts;
use parking_lot::RwLock;
use pin_project::pin_project;
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use thiserror::Error;
use tokio::{sync::oneshot, time};

#[derive(Clone, Debug)]
pub struct NewSetStreamTimeoutsExtension<N> {
    inner: N,
}

/// A request extension set on HTTP requests that expresses deadlines to be
/// enforced by the proxy.
#[derive(Clone, Debug, Default)]
pub struct StreamTimeouts {
    /// The maximum amount of time between the body of the request being fully
    /// flushed and the response headers being received.
    pub response_headers: Option<time::Duration>,

    /// The maximum amount of time between the body of the request being fully
    /// flushed (or the response headers being received, if that occurs first)
    /// and the response being fully received.
    pub response_end: Option<time::Duration>,

    /// The maximum amount of time the stream may be idle.
    pub idle: Option<time::Duration>,

    /// Limits the total time the stream may be active in the proxy.
    pub limit: Option<StreamLifetime>,
}

#[derive(Clone, Copy, Debug)]
pub struct StreamLifetime {
    /// The deadline for the stream.
    pub deadline: time::Instant,
    /// The maximum amount of time the stream may be active, used for error reporting.
    pub lifetime: time::Duration,
}

#[derive(Clone, Debug)]
pub struct SetStreamTimeoutsExtension<S> {
    inner: S,
    timeouts: Timeouts,
}

#[derive(Clone, Debug)]
pub struct EnforceTimeouts<S> {
    inner: S,
}

#[derive(Clone, Copy, Debug, Error)]
#[error("timed out waiting for response headers: {0:?}")]
pub struct ResponseHeadersTimeout(time::Duration);

#[derive(Clone, Copy, Debug, Error)]
#[error("timed out waiting for response stream: {0:?}")]
pub struct ResponseStreamTimeout(time::Duration);

#[derive(Clone, Copy, Debug, Error)]
#[error("stream lifetime exceeded: {0:?}")]
pub struct StreamLifetimeExceeded(time::Duration);

#[derive(Clone, Copy, Debug, Error)]
#[error("stream timed out due to idleness: {0:?}")]
pub struct StreamIdleTimeout(time::Duration);

#[derive(Clone, Copy, Debug, Error)]
pub enum StreamTimeoutError {
    #[error(transparent)]
    ResponseHeaders(#[from] ResponseHeadersTimeout),

    #[error(transparent)]
    ResponseStream(#[from] ResponseStreamTimeout),

    #[error(transparent)]
    Lifetime(#[from] StreamLifetimeExceeded),

    #[error(transparent)]
    Idle(#[from] StreamIdleTimeout),
}

#[derive(Debug)]
#[pin_project]
pub struct ResponseFuture<F> {
    #[pin]
    inner: F,
    #[pin]
    deadline: Option<time::Sleep>,
    error: Option<StreamTimeoutError>,

    #[pin]
    request_flushed: Option<oneshot::Receiver<time::Instant>>,
    request_flushed_at: Option<time::Instant>,

    idle: Option<(IdleTimestamp, time::Duration)>,

    timeouts: StreamTimeouts,
}

#[derive(Debug, Default)]
#[pin_project]
pub struct RequestBody<B> {
    #[pin]
    inner: B,

    #[pin]
    deadline: Option<time::Sleep>,
    error: Option<StreamTimeoutError>,

    idle: Option<Idle>,

    request_flushed: Option<oneshot::Sender<time::Instant>>,
}

#[derive(Debug, Default)]
#[pin_project]
pub struct ResponseBody<B> {
    #[pin]
    inner: B,

    #[pin]
    deadline: Option<time::Sleep>,
    error: Option<StreamTimeoutError>,

    idle: Option<Idle>,

    timeouts: StreamTimeouts,
}

type IdleTimestamp = Arc<RwLock<time::Instant>>;

#[derive(Debug)]
struct Idle {
    sleep: Pin<Box<time::Sleep>>,
    last_update: IdleTimestamp,
    timeout: time::Duration,
}

// === impl NewSetDeadlines ===

impl<N> NewSetStreamTimeoutsExtension<N> {
    pub fn layer() -> impl svc::Layer<N, Service = Self> + Clone {
        svc::layer::mk(|inner| Self { inner })
    }
}

impl<T, N> svc::NewService<T> for NewSetStreamTimeoutsExtension<N>
where
    N: svc::NewService<T>,
    T: svc::Param<Timeouts>,
{
    type Service = SetStreamTimeoutsExtension<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let timeouts = target.param();
        SetStreamTimeoutsExtension {
            timeouts,
            inner: self.inner.new_service(target),
        }
    }
}

// === impl SetDeadlines ===

impl<B, S> svc::Service<http::Request<B>> for SetStreamTimeoutsExtension<S>
where
    S: svc::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        // TODO(ver): Use request headers, like grpc-timeout, to set deadlines.
        let _prior = req.extensions_mut().insert(StreamTimeouts {
            response_headers: self.timeouts.response,
            response_end: self.timeouts.response,
            idle: self.timeouts.idle,
            limit: self.timeouts.stream.map(|lifetime| StreamLifetime {
                lifetime,
                deadline: time::Instant::now() + lifetime,
            }),
        });
        debug_assert!(_prior.is_none(), "Timeouts must only be configured once");
        self.inner.call(req)
    }
}

// === impl EnforceTimeouts ===

impl<S> EnforceTimeouts<S> {
    pub fn layer() -> impl svc::Layer<S, Service = Self> + Clone {
        svc::layer::mk(|inner| Self { inner })
    }
}

impl<S, B, RspB> svc::Service<http::Request<B>> for EnforceTimeouts<S>
where
    S: svc::Service<http::Request<RequestBody<B>>, Response = http::Response<RspB>>,
    S::Error: Into<Error>,
{
    type Response = http::Response<ResponseBody<RspB>>;
    type Error = Error;
    type Future = ResponseFuture<S::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        let timeouts = req
            .extensions()
            .get::<StreamTimeouts>()
            .cloned()
            .unwrap_or_default();

        let (req_idle, rsp_idle) = if let Some(timeout) = timeouts.idle {
            let last_update = Arc::new(RwLock::new(time::Instant::now()));
            let req = Idle {
                sleep: Box::pin(time::sleep(timeout)),
                last_update: last_update.clone(),
                timeout,
            };
            (Some(req), Some((last_update, timeout)))
        } else {
            (None, None)
        };

        let (tx, rx) = oneshot::channel();
        let inner = self.inner.call(req.map(move |inner| {
            RequestBody {
                inner,
                request_flushed: Some(tx),
                deadline: timeouts.limit.map(|l| time::sleep_until(l.deadline)),
                error: timeouts
                    .limit
                    .map(|l| StreamLifetimeExceeded(l.lifetime).into()),
                idle: req_idle,
            }
        }));
        ResponseFuture {
            inner,
            deadline: timeouts.limit.map(|l| time::sleep_until(l.deadline)),
            error: timeouts
                .limit
                .map(|l| StreamLifetimeExceeded(l.lifetime).into()),
            request_flushed: Some(rx),
            request_flushed_at: None,
            timeouts,
            idle: rsp_idle,
        }
    }
}

// === impl ResponseFuture ===

impl<RspB, E, F> Future for ResponseFuture<F>
where
    F: Future<Output = Result<http::Response<RspB>, E>>,
    E: Into<Error>,
{
    type Output = Result<http::Response<ResponseBody<RspB>>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        // Mark the time at which the request body was fully flushed and adjust
        // the response deadline as necessary.
        if let Some(flushed) = this.request_flushed.as_mut().as_pin_mut() {
            if let Poll::Ready(res) = flushed.poll(cx) {
                *this.request_flushed = None;
                if let Ok(flush) = res {
                    *this.request_flushed_at = Some(flush);

                    if let Some(timeout) = this.timeouts.response_headers {
                        let headers_by = flush + timeout;
                        if let Some(deadline) = this.deadline.as_mut().as_pin_mut() {
                            if headers_by < deadline.deadline() {
                                *this.error = Some(ResponseHeadersTimeout(timeout).into());
                                deadline.reset(headers_by);
                            }
                        } else {
                            this.deadline.set(Some(time::sleep_until(headers_by)));
                        }
                    }
                }
            }
        }

        // Poll for the response headers.
        let rsp = match this.inner.poll(cx) {
            Poll::Ready(Ok(rsp)) => rsp,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e.into())),
            Poll::Pending => {
                // If the response headers are not ready, check the deadline and
                // return an error if it is exceeded.
                if let Some(deadline) = this.deadline.as_pin_mut() {
                    if deadline.poll(cx).is_ready() {
                        // TODO telemetry
                        return Poll::Ready(Err(this
                            .error
                            .expect("error must be set when deadline is set")
                            .into()));
                    }
                }
                return Poll::Pending;
            }
        };

        // We've received response headers, so we prepare the response body to
        // timeout. We use the more restrictive of the response-end timeout (as
        // measured since the request body was fully flushed) and the stream
        // lifetime limit.
        let start = this.request_flushed_at.unwrap_or_else(time::Instant::now);
        let (deadline, error) = match (this.timeouts.response_end, this.timeouts.limit) {
            (Some(eos), Some(lim)) if start + eos < lim.deadline => {
                (Some(start + eos), Some(ResponseStreamTimeout(eos).into()))
            }
            (Some(_), Some(lim)) => (
                Some(lim.deadline),
                Some(StreamLifetimeExceeded(lim.lifetime).into()),
            ),
            (Some(eos), None) => (Some(start + eos), Some(ResponseStreamTimeout(eos).into())),
            (None, Some(lim)) => (
                Some(lim.deadline),
                Some(StreamLifetimeExceeded(lim.lifetime).into()),
            ),
            (None, None) => (None, None),
        };

        // Share the idle state across request and response bodies
        let idle = this.idle.take().map(|(last_update, timeout)| Idle {
            sleep: Box::pin(time::sleep(timeout)),
            last_update,
            timeout,
        });

        Poll::Ready(Ok(rsp.map(move |inner| ResponseBody {
            inner,
            deadline: deadline.map(time::sleep_until),
            error,
            idle,
            timeouts: this.timeouts.clone(),
        })))
    }
}

// === impl RequestBody ===

impl<B> http::HttpBody for RequestBody<B>
where
    B: http::HttpBody<Error = Error>,
{
    type Data = B::Data;
    type Error = Error;

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let this = self.project();

        if let Poll::Ready(res) = this.inner.poll_data(cx) {
            if let Some(idle) = this.idle {
                idle.reset(time::Instant::now());
            }
            return Poll::Ready(res);
        }

        if let Poll::Ready(e) = poll_body_timeout(this.deadline, this.error, this.idle, cx) {
            // TODO telemetry
            return Poll::Ready(Some(Err(Error::from(e))));
        }

        Poll::Pending
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        let this = self.project();

        if let Poll::Ready(res) = this.inner.poll_trailers(cx) {
            let now = time::Instant::now();
            if let Some(idle) = this.idle {
                idle.reset(now);
            }
            if let Some(tx) = this.request_flushed.take() {
                let _ = tx.send(now);
            }
            return Poll::Ready(res);
        }

        if let Poll::Ready(e) = poll_body_timeout(this.deadline, this.error, this.idle, cx) {
            // TODO telemetry
            return Poll::Ready(Err(Error::from(e)));
        }

        Poll::Pending
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}

// === impl ResponseBody ===

impl<B> http::HttpBody for ResponseBody<B>
where
    B: http::HttpBody<Error = Error>,
{
    type Data = B::Data;
    type Error = Error;

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let this = self.project();

        if let Poll::Ready(res) = this.inner.poll_data(cx) {
            if let Some(idle) = this.idle {
                idle.reset(time::Instant::now());
            }
            return Poll::Ready(res);
        }

        if let Poll::Ready(e) = poll_body_timeout(this.deadline, this.error, this.idle, cx) {
            // TODO telemetry
            return Poll::Ready(Some(Err(Error::from(e))));
        }

        Poll::Pending
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        let this = self.project();

        if let Poll::Ready(res) = this.inner.poll_trailers(cx) {
            if let Some(idle) = this.idle {
                idle.reset(time::Instant::now());
            };
            return Poll::Ready(res);
        }

        if let Poll::Ready(e) = poll_body_timeout(this.deadline, this.error, this.idle, cx) {
            // TODO telemetry
            return Poll::Ready(Err(Error::from(e)));
        }

        Poll::Pending
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}

fn poll_body_timeout(
    mut deadline: Pin<&mut Option<time::Sleep>>,
    error: &mut Option<StreamTimeoutError>,
    idle: &mut Option<Idle>,
    cx: &mut Context<'_>,
) -> Poll<StreamTimeoutError> {
    if let Some(d) = deadline.as_mut().as_pin_mut() {
        if d.poll(cx).is_ready() {
            deadline.set(None); // Prevent polling again.
            return Poll::Ready(
                error
                    .take()
                    .expect("deadline must be set when error is set"),
            );
        }
    }

    if let Some(idle) = idle {
        if let Poll::Ready(e) = idle.poll_idle(cx) {
            return Poll::Ready(e.into());
        }
    }

    Poll::Pending
}

// === impl Idle ===

impl Idle {
    fn reset(&mut self, now: time::Instant) {
        self.sleep.as_mut().reset(now + self.timeout);
        *self.last_update.write() = now;
    }

    fn poll_idle(&mut self, cx: &mut Context<'_>) -> Poll<StreamIdleTimeout> {
        loop {
            if self.sleep.poll_unpin(cx).is_pending() {
                return Poll::Pending;
            }
            let now = time::Instant::now();
            let expiry = *self.last_update.read() + self.timeout;
            if expiry <= now {
                return Poll::Ready(StreamIdleTimeout(self.timeout));
            }
            self.sleep.as_mut().reset(expiry);
        }
    }
}
