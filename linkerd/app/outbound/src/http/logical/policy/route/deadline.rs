use futures::FutureExt;
use linkerd_app_core::{proxy::http, svc, Error, Result};
use linkerd_proxy_client_policy::http::Timeouts;
use parking_lot::RwLock;
use pin_project::pin_project;
use std::{future::Future, pin::Pin, sync::Arc, task};
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
    /// flushed and the response being fully received.
    pub response_end: Option<time::Duration>,

    /// The maximum amount of time the stream may be idle.
    pub idle: Option<time::Duration>,

    /// The time by which the entire stream must be complete.
    pub deadline: Option<time::Instant>,
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

#[derive(Debug)]
#[pin_project]
pub struct ResponseFuture<F> {
    #[pin]
    inner: F,
    #[pin]
    deadline: Option<time::Sleep>,

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

    idle: Option<Idle>,

    #[pin]
    request_flushed: Option<oneshot::Receiver<time::Instant>>,
    request_flushed_at: Option<time::Instant>,

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
    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        // TODO(ver): Use request headers, like grpc-timeout, to set deadlines.
        let _prior = req.extensions_mut().insert(StreamTimeouts {
            response_headers: None,
            response_end: self.timeouts.response,
            idle: self.timeouts.idle,
            deadline: self.timeouts.stream.map(|t| time::Instant::now() + t),
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
    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> task::Poll<Result<(), Self::Error>> {
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
        let inner = self.inner.call(req.map(move |inner| RequestBody {
            inner,
            request_flushed: Some(tx),
            deadline: timeouts.deadline.map(time::sleep_until),
            idle: req_idle,
        }));
        ResponseFuture {
            inner,
            deadline: timeouts.deadline.map(time::sleep_until),
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

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> task::Poll<Self::Output> {
        let mut this = self.project();

        // Mark the time at which the request body was fully flushed and adjust
        // the response deadline as necessary.
        if let Some(flushed) = this.request_flushed.as_mut().as_pin_mut() {
            if let task::Poll::Ready(res) = flushed.poll(cx) {
                *this.request_flushed = None;
                if let Ok(flush) = res {
                    *this.request_flushed_at = Some(flush);

                    if let Some(rh) = this.timeouts.response_headers {
                        let headers_by = flush + rh;
                        if let Some(deadline) = this.deadline.as_mut().as_pin_mut() {
                            if headers_by < deadline.deadline() {
                                deadline.reset(headers_by);
                            }
                        } else {
                            this.deadline.set(Some(time::sleep_until(headers_by)));
                        }
                    }
                }
            }
        }

        let rsp = match this.inner.poll(cx) {
            task::Poll::Ready(Ok(rsp)) => rsp,
            task::Poll::Ready(Err(e)) => return task::Poll::Ready(Err(e.into())),
            task::Poll::Pending => {
                if let Some(deadline) = this.deadline.as_pin_mut() {
                    if deadline.poll(cx).is_ready() {
                        return task::Poll::Ready(Err(Error::from("TODO")));
                    }
                }
                return task::Poll::Pending;
            }
        };

        let idle = this.idle.take().map(|(last_update, timeout)| Idle {
            sleep: Box::pin(time::sleep(timeout)),
            last_update,
            timeout,
        });

        let deadline = this
            .request_flushed_at
            .and_then(|t| this.timeouts.response_end.map(|d| t + d))
            .min(this.timeouts.deadline)
            .map(time::sleep_until);

        task::Poll::Ready(Ok(rsp.map(move |inner| ResponseBody {
            inner,
            deadline,
            idle,
            request_flushed: this.request_flushed.take(),
            request_flushed_at: this.request_flushed_at.take(),
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
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Option<Result<Self::Data, Self::Error>>> {
        let this = self.project();

        if let task::Poll::Ready(res) = this.inner.poll_data(cx) {
            if let Some(idle) = this.idle.as_mut() {
                let now = time::Instant::now();
                idle.sleep.as_mut().reset(now + idle.timeout);
                *idle.last_update.write() = now;
            }
            return task::Poll::Ready(res);
        }

        if let Some(deadline) = this.deadline.as_pin_mut() {
            if deadline.poll(cx).is_ready() {
                // FIXME
                return task::Poll::Ready(Some(Err(Error::from("FIXME"))));
            }
        }

        if let Some(idle) = this.idle.as_mut() {
            if idle.poll_idle(cx).is_ready() {
                // FIXME
                return task::Poll::Ready(Some(Err(Error::from("FIXME"))));
            }
        }

        task::Poll::Pending
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        let this = self.project();

        if let task::Poll::Ready(res) = this.inner.poll_trailers(cx) {
            let now = time::Instant::now();
            if let Some(idle) = this.idle.as_mut() {
                idle.sleep.as_mut().reset(now + idle.timeout);
                *idle.last_update.write() = now;
            };
            if let Some(tx) = this.request_flushed.take() {
                let _ = tx.send(now);
            }
            return task::Poll::Ready(res);
        }

        if let Some(deadline) = this.deadline.as_pin_mut() {
            if deadline.poll(cx).is_ready() {
                // FIXME
                return task::Poll::Ready(Err(Error::from("FIXME")));
            }
        }

        if let Some(idle) = this.idle.as_mut() {
            if idle.poll_idle(cx).is_ready() {
                // FIXME
                return task::Poll::Ready(Err(Error::from("FIXME")));
            }
        }

        task::Poll::Pending
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
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Option<Result<Self::Data, Self::Error>>> {
        let mut this = self.project();

        if let task::Poll::Ready(res) = this.inner.poll_data(cx) {
            if let Some(idle) = this.idle.as_mut() {
                let now = time::Instant::now();
                idle.sleep.as_mut().reset(now + idle.timeout);
                *idle.last_update.write() = now;
            }
            return task::Poll::Ready(res);
        }

        if Self::poll_timeout(
            this.deadline.as_mut(),
            this.request_flushed.as_mut(),
            this.request_flushed_at,
            this.timeouts,
            cx,
        )
        .is_ready()
        {
            // FIXME
            return task::Poll::Ready(Some(Err(Error::from("FIXME"))));
        }

        if let Some(idle) = this.idle.as_mut() {
            if idle.poll_idle(cx).is_ready() {
                // FIXME
                return task::Poll::Ready(Some(Err(Error::from("FIXME"))));
            }
        }

        task::Poll::Pending
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        let mut this = self.project();

        if let task::Poll::Ready(res) = this.inner.poll_trailers(cx) {
            let now = time::Instant::now();
            if let Some(idle) = this.idle.as_mut() {
                idle.sleep.as_mut().reset(now + idle.timeout);
                *idle.last_update.write() = now;
            };
            return task::Poll::Ready(res);
        }

        if Self::poll_timeout(
            this.deadline.as_mut(),
            this.request_flushed.as_mut(),
            this.request_flushed_at,
            this.timeouts,
            cx,
        )
        .is_ready()
        {
            // FIXME
            return task::Poll::Ready(Err(Error::from("FIXME")));
        }

        if let Some(idle) = this.idle.as_mut() {
            if idle.poll_idle(cx).is_ready() {
                // FIXME
                return task::Poll::Ready(Err(Error::from("FIXME")));
            }
        }

        task::Poll::Pending
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}

impl<B> ResponseBody<B> {
    fn poll_timeout(
        mut deadline: Pin<&mut Option<time::Sleep>>,
        mut request_flushed: Pin<&mut Option<oneshot::Receiver<time::Instant>>>,
        request_flushed_at: &mut Option<time::Instant>,
        timeouts: &StreamTimeouts,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<()> {
        if let Some(flushed) = request_flushed.as_mut().as_pin_mut() {
            if let task::Poll::Ready(res) = flushed.poll(cx) {
                request_flushed.set(None);
                if let Ok(flush) = res {
                    *request_flushed_at = Some(flush);

                    if let Some(timeout) = timeouts.response_end {
                        let eos_by = flush + timeout;
                        if let Some(deadline) = deadline.as_mut().as_pin_mut() {
                            if eos_by < deadline.deadline() {
                                deadline.reset(eos_by);
                            }
                        } else {
                            deadline.set(Some(time::sleep_until(eos_by)));
                        }
                    }
                }
            }
        }

        if let Some(deadline) = deadline.as_pin_mut() {
            if deadline.poll(cx).is_ready() {
                return task::Poll::Ready(());
            }
        }

        task::Poll::Pending
    }
}

// === impl Idle ===

impl Idle {
    fn poll_idle(&mut self, cx: &mut task::Context<'_>) -> task::Poll<()> {
        loop {
            if self.sleep.poll_unpin(cx).is_pending() {
                return task::Poll::Pending;
            }
            let now = time::Instant::now();
            let expiry = *self.last_update.read() + self.timeout;
            if expiry <= now {
                return task::Poll::Ready(());
            }
            self.sleep.as_mut().reset(expiry);
        }
    }
}
