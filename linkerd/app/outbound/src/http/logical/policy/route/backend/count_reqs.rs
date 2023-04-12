use linkerd_app_core::{metrics::Counter, svc};
use std::{
    sync::Arc,
    task::{Context, Poll},
};

#[derive(Clone, Debug, Default)]
pub struct RequestCount(pub Arc<Counter>);

#[derive(Clone, Debug)]
pub struct NewCountRequests<X, N> {
    inner: N,
    extract: X,
}

#[derive(Clone, Debug)]
pub struct CountRequests<S> {
    inner: S,
    requests: Arc<Counter>,
}

// === impl NewCountRequests ===

impl<X: Clone, N> NewCountRequests<X, N> {
    pub fn new(extract: X, inner: N) -> Self {
        Self { extract, inner }
    }

    pub fn layer_via(extract: X) -> impl svc::Layer<N, Service = Self> + Clone {
        svc::layer::mk(move |inner| Self::new(extract.clone(), inner))
    }
}

impl<T, X, N> svc::NewService<T> for NewCountRequests<X, N>
where
    X: svc::ExtractParam<RequestCount, T>,
    N: svc::NewService<T>,
{
    type Service = CountRequests<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let RequestCount(counter) = self.extract.extract_param(&target);
        let inner = self.inner.new_service(target);
        CountRequests::new(counter, inner)
    }
}

// === impl CountRequests ===

impl<S> CountRequests<S> {
    fn new(requests: Arc<Counter>, inner: S) -> Self {
        Self { requests, inner }
    }
}

impl<B, S> svc::Service<http::Request<B>> for CountRequests<S>
where
    S: svc::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        self.requests.incr();
        self.inner.call(req)
    }
}
