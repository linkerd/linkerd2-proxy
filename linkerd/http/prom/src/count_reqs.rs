use linkerd_stack as svc;
use prometheus_client::{
    encoding::EncodeLabelSet,
    metrics::{counter::Counter, family::Family},
    registry::Registry,
};
use std::task::{Context, Poll};

#[derive(Clone, Debug)]
pub struct RequestCountFamilies<L: Clone>(Family<L, Counter>);

#[derive(Clone, Debug)]
pub struct RequestCount(Counter);

#[derive(Clone, Debug)]
pub struct NewCountRequests<X, N> {
    inner: N,
    extract: X,
}

#[derive(Clone, Debug)]
pub struct CountRequests<S> {
    inner: S,
    requests: Counter,
}

impl<L> RequestCountFamilies<L>
where
    L: EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone + Send + Sync + 'static,
{
    pub fn register(registry: &mut Registry) -> Self {
        let requests = Family::default();
        registry.register(
            "requests",
            "The total number of requests dispatched",
            requests.clone(),
        );
        Self(requests)
    }

    pub fn metrics(&self, labels: &L) -> RequestCount {
        RequestCount(self.0.get_or_create(labels).clone())
    }
}

// === impl NewCountRequests ===

impl<X: Clone, N> NewCountRequests<X, N> {
    pub fn new(extract: X, inner: N) -> Self {
        Self { extract, inner }
    }

    pub fn layer_via(extract: X) -> impl svc::layer::Layer<N, Service = Self> + Clone {
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
    fn new(requests: Counter, inner: S) -> Self {
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
        self.requests.inc();
        self.inner.call(req)
    }
}

impl<L> Default for RequestCountFamilies<L>
where
    L: EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone + Send + Sync + 'static,
{
    fn default() -> Self {
        Self(Family::default())
    }
}

impl RequestCount {
    pub fn get(&self) -> u64 {
        self.0.get()
    }
}
