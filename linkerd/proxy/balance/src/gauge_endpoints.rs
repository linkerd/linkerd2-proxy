use linkerd_metrics::Gauge;
use linkerd_stack::{layer, ExtractParam, NewService, Service};
use std::{
    sync::Arc,
    task::{Context, Poll},
};

/// Parameter that provides a per-balancer endpoint gauge.
#[derive(Clone, Debug, Default)]
pub struct EndpointsGauges {
    pub ready: Arc<Gauge>,
    pub pending: Arc<Gauge>,
}

/// A [`NewService`] that builds [`NewGaugeBalancerEndpoint`]s with an
/// endpoint gauge.
#[derive(Clone, Debug, Default)]
pub struct NewGaugeEndpoints<X, N> {
    inner: N,
    extract: X,
}

/// A [`NewService`] that builds [`GaugeBalancerEndpoint`]s with an
/// endpoint gauge.
#[derive(Clone, Debug, Default)]
pub struct NewGaugeBalancerEndpoint<N> {
    inner: N,
    gauge: EndpointsGauges,
}

/// A [`Service`] that decrements an endpoint gauge when dropped.
#[derive(Debug)]
pub struct GaugeBalancerEndpoint<S> {
    inner: S,
    gauge: EndpointsGauges,
    state: Poll<()>,
}

/// === impl NewGaugeBalancerEndpointSet ===

impl<X: Clone, N> NewGaugeEndpoints<X, N> {
    pub fn new(extract: X, inner: N) -> Self {
        Self { extract, inner }
    }

    pub fn layer_via(extract: X) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(extract.clone(), inner))
    }
}

impl<N> NewGaugeEndpoints<(), N> {
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        Self::layer_via(())
    }
}

impl<T, X, N> NewService<T> for NewGaugeEndpoints<X, N>
where
    X: ExtractParam<EndpointsGauges, T>,
    N: NewService<T>,
{
    type Service = NewGaugeBalancerEndpoint<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let gauge = self.extract.extract_param(&target);
        let inner = self.inner.new_service(target);
        NewGaugeBalancerEndpoint { inner, gauge }
    }
}

/// === impl NewGaugeBalancerEndpoint ===

impl<T, N> NewService<T> for NewGaugeBalancerEndpoint<N>
where
    N: NewService<T>,
{
    type Service = GaugeBalancerEndpoint<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let gauge = self.gauge.clone();
        let inner = self.inner.new_service(target);
        gauge.pending.incr();
        GaugeBalancerEndpoint {
            inner,
            gauge,
            state: Poll::Pending,
        }
    }
}

/// === impl GaugeBalancerEndpoint ===

impl<Req, S> Service<Req> for GaugeBalancerEndpoint<S>
where
    S: Service<Req>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let poll = self.inner.poll_ready(cx);
        if self.state.is_ready() && poll.is_pending() {
            self.gauge.pending.incr();
            self.gauge.ready.decr();
            self.state = Poll::Pending;
        } else if self.state.is_pending() && poll.is_ready() {
            self.gauge.pending.decr();
            self.gauge.ready.incr();
            self.state = Poll::Ready(());
        }
        poll
    }

    #[inline]
    fn call(&mut self, req: Req) -> Self::Future {
        self.inner.call(req)
    }
}

impl<S> Drop for GaugeBalancerEndpoint<S> {
    fn drop(&mut self) {
        if self.state.is_pending() {
            self.gauge.pending.decr();
        } else {
            self.gauge.ready.decr();
        }
    }
}
