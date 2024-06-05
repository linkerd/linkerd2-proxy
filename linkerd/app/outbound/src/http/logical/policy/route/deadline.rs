use linkerd_app_core::svc;
use linkerd_proxy_client_policy::http::Timeouts;
use tokio::time;

#[derive(Clone, Debug)]
pub struct NewSetDeadlines<N> {
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
pub struct SetDeadlines<S> {
    inner: S,
    timeouts: Timeouts,
}

#[derive(Clone, Debug)]
pub struct EnforceDeadlines<S> {
    inner: S,
}

// === impl NewSetDeadlines ===

impl<N> NewSetDeadlines<N> {
    pub fn layer() -> impl svc::Layer<N, Service = Self> + Clone {
        svc::layer::mk(|inner| Self { inner })
    }
}

impl<T, N> svc::NewService<T> for NewSetDeadlines<N>
where
    N: svc::NewService<T>,
    T: svc::Param<Timeouts>,
{
    type Service = SetDeadlines<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let timeouts = target.param();
        SetDeadlines {
            timeouts,
            inner: self.inner.new_service(target),
        }
    }
}

// === impl SetDeadlines ===

impl<B, S> svc::Service<http::Request<B>> for SetDeadlines<S>
where
    S: svc::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        let _prior = req.extensions_mut().insert(StreamTimeouts {
            response_headers: None,
            response_end: self.timeouts.response,
            idle: self.timeouts.idle,
            deadline: self.timeouts.stream.map(|t| time::Instant::now() + t),
        });
        debug_assert!(_prior.is_none(), "Timeouts must only be configured onc");
        self.inner.call(req)
    }
}

// === impl EnforceDeadlines ===

impl<S> EnforceDeadlines<S> {
    pub fn layer() -> impl svc::Layer<S, Service = Self> + Clone {
        svc::layer::mk(|inner| Self { inner })
    }
}

impl<S, B> svc::Service<http::Request<B>> for EnforceDeadlines<S>
where
    S: svc::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        let _timeouts = req.extensions().get::<StreamTimeouts>().cloned();
        // TODO
        // - Request body timeouts
        //   - stream timeout
        //   - idle timeout
        // - Response headers timeouts
        //   - stream timeout
        //   - idle timeout
        // - Response body timeouts
        //   - stream timeout
        //   - idle timeout
        //   - response end timeout
        self.inner.call(req)
    }
}
