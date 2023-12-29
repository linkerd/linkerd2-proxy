use linkerd_stack::{NewService, Service};
use prometheus_client::{
    metrics::{family::Family, gauge::Gauge},
    registry::Registry,
};
use std::task::{Context, Poll};

#[derive(Clone, Debug)]
pub struct EndpointsGaugesFamilies<L>(Family<StateLabels<L>, Gauge>);

/// Parameter that provides a per-balancer endpoint gauge.
#[derive(Clone, Debug, Default)]
pub struct EndpointsGauges {
    ready: Gauge,
    pending: Gauge,
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
    state: Option<Poll<()>>,
}

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
struct StateLabels<L> {
    state: StateLabel,
    labels: L,
}

#[derive(
    Copy, Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelValue,
)]
#[allow(non_camel_case_types)]
enum StateLabel {
    ready,
    pending,
}

impl<L> EndpointsGaugesFamilies<L>
where
    L: prometheus_client::encoding::EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone + Send + Sync + 'static,
{
    pub fn register(reg: &mut Registry) -> Self {
        let family = Family::default();
        reg.register(
            "endpoints",
            "The number of endpoints currently in a HTTP request balancer",
            family.clone(),
        );
        Self(family)
    }

    pub fn metrics(&self, labels: &L) -> EndpointsGauges {
        let ready = self
            .0
            .get_or_create(&StateLabels {
                state: StateLabel::ready,
                labels: labels.clone(),
            })
            .clone();
        let pending = self
            .0
            .get_or_create(&StateLabels {
                state: StateLabel::pending,
                labels: labels.clone(),
            })
            .clone();
        EndpointsGauges { ready, pending }
    }
}

impl<L: prometheus_client::encoding::EncodeLabelSet> prometheus_client::encoding::EncodeLabelSet
    for StateLabels<L>
{
    fn encode(
        &self,
        mut enc: prometheus_client::encoding::LabelSetEncoder<'_>,
    ) -> std::fmt::Result {
        use prometheus_client::encoding::EncodeLabel;
        ("endpoint_state", self.state).encode(enc.encode_label())?;
        self.labels.encode(enc)
    }
}

// === impl NewGaugeBalancerEndpoint ===

impl<N> NewGaugeBalancerEndpoint<N> {
    pub fn new(gauge: EndpointsGauges, inner: N) -> Self {
        Self { inner, gauge }
    }
}

impl<T, N> NewService<T> for NewGaugeBalancerEndpoint<N>
where
    N: NewService<T>,
{
    type Service = GaugeBalancerEndpoint<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let gauge = self.gauge.clone();
        let inner = self.inner.new_service(target);
        GaugeBalancerEndpoint {
            inner,
            gauge,
            state: None,
        }
    }
}

// === impl GaugeBalancerEndpoint ===

impl<Req, S> Service<Req> for GaugeBalancerEndpoint<S>
where
    S: Service<Req>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let poll = self.inner.poll_ready(cx);
        match (self.state, &poll) {
            (Some(Poll::Ready(())), Poll::Pending) => {
                self.gauge.ready.dec();
                self.gauge.pending.inc();
                self.state = Some(Poll::Pending);
            }
            (Some(Poll::Pending), Poll::Ready(Ok(()))) => {
                self.gauge.pending.dec();
                self.gauge.ready.inc();
                self.state = Some(Poll::Ready(()));
            }
            (None, Poll::Pending) => {
                self.gauge.pending.inc();
                self.state = Some(Poll::Pending);
            }
            (None, Poll::Ready(Ok(()))) => {
                self.gauge.ready.inc();
                self.state = Some(Poll::Ready(()));
            }
            _ => {}
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
        if let Some(s) = self.state.take() {
            if s.is_ready() {
                self.gauge.ready.dec();
            } else {
                self.gauge.pending.dec();
            }
        }
    }
}
