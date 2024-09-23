//! A Tower middleware for counting requests processed by a service.

use linkerd_stack as svc;
use prometheus_client::{
    encoding::EncodeLabelSet,
    metrics::{counter::Counter, family::Family},
    registry::Registry,
};
use std::task::{Context, Poll};

/// A [`Family`] of counters with `L`-encoded labels.
///
/// See [`EncodeLabelSet`] for more information about encoding labels.
#[derive(Clone, Debug)]
pub struct RequestCountFamilies<L>(Family<L, Counter>);

// A single [`Counter`] that tracks the number of requests.
#[derive(Clone, Debug)]
pub struct RequestCount(Counter);

/// A [`NewService<T>`][svc::NewService] that creates [`RequestCount`] services.
#[derive(Clone, Debug)]
pub struct NewCountRequests<X, N> {
    /// The inner [`NewService<T>`][svc::NewService].
    inner: N,
    /// The [`ExtractParam<P, T>`][svc::ExtractParam] strategy for obtaining our parameters.
    extract: X,
}

/// Counts requests for an inner `S`-typed [`Service`][svc::Service].
///
/// This will increment its counter when [`call()`][svc::Service::call]ed, before calling the inner
/// service.
#[derive(Clone, Debug)]
pub struct CountRequests<S> {
    inner: S,
    requests: Counter,
}

// === impl NewCountRequests ===

impl<X: Clone, N> NewCountRequests<X, N> {
    pub fn new(extract: X, inner: N) -> Self {
        Self { extract, inner }
    }

    /// Returns a [`Layer`] that counts requests.
    ///
    /// This uses an `X`-typed [`ExtractParam`] implementation to extract [`RequestCount`] from a
    /// `T`-typed target.
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
        let rc = self.extract.extract_param(&target);
        let inner = self.inner.new_service(target);
        CountRequests::new(rc, inner)
    }
}

// === impl CountRequests ===

impl<S> CountRequests<S> {
    pub(crate) fn new(RequestCount(requests): RequestCount, inner: S) -> Self {
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
        // We received a request! Increment the counter and then call the inner service `S`.
        self.requests.inc();
        self.inner.call(req)
    }
}

// === impl RequestCountFamilies ===

impl<L> Default for RequestCountFamilies<L>
where
    L: EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone,
{
    fn default() -> Self {
        Self(Family::default())
    }
}

impl<L> RequestCountFamilies<L>
where
    L: EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone + Send + Sync + 'static,
{
    /// Registers this family of counters with the given [`Registry`].
    pub fn register(registry: &mut Registry) -> Self {
        let requests = Family::default();
        registry.register(
            "requests",
            "The total number of requests dispatched",
            requests.clone(),
        );
        Self(requests)
    }

    /// Returns a [`RequestCount`] for the given label set.
    pub fn metrics(&self, labels: &L) -> RequestCount {
        RequestCount(self.0.get_or_create(labels).clone())
    }
}

// === impl RequestCount ===

impl RequestCount {
    /// Returns the current value of the counter.
    pub fn get(&self) -> u64 {
        self.0.get()
    }
}
