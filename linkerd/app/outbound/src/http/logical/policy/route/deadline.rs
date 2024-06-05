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
pub struct StreamDeadlines {
    pub response_headers: Option<time::Instant>,
    pub response_end: Option<time::Instant>,
    pub stream_end: Option<time::Instant>,
    pub idle_timeout: Option<time::Duration>,
}

#[derive(Clone, Debug)]
pub struct SetDeadlines<S> {
    inner: S,
    timeouts: Timeouts,
}

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
        req.extensions_mut().insert(StreamDeadlines {
            response_headers: None,
            response_end: self.timeouts.response.map(|t| time::Instant::now() + t),
            stream_end: self.timeouts.stream.map(|t| time::Instant::now() + t),
            idle_timeout: self.timeouts.idle,
        });
        self.inner.call(req)
    }
}
