use linkerd_stack as svc;
use prometheus_client::{
    encoding::EncodeLabelSet,
    metrics::{
        family::{Family, MetricConstructor},
        histogram::Histogram,
    },
    registry::{Registry, Unit},
};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::time;

#[derive(Clone, Debug)]
pub struct ResponseMetricsFamilies<L, H> {
    // errors: Family<L, Counter>,
    request_durations: RequestDurationsFamily<L, H>,
}

type RequestDurationsFamily<L, H> = Family<Labels<L>, Histogram, H>;

#[derive(Clone, Debug)]
pub struct ResponseMetrics<L, H> {
    labels: L,
    // errors: Counter,
    request_durations: Family<Labels<L>, Histogram, H>,
}

#[derive(Clone, Debug)]
pub struct NewResponseMetrics<X, L, H, N> {
    inner: N,
    extract: X,
    _marker: std::marker::PhantomData<fn() -> (L, H)>,
}

#[derive(Clone, Debug)]
pub struct ResponseMetricsService<L, H, S> {
    inner: S,
    metrics: ResponseMetrics<L, H>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Labels<L> {
    status_code: u16,
    common: L,
}

// Define the ResponseFuture type
#[pin_project::pin_project]
pub struct ResponseFuture<L, H, F> {
    #[pin]
    future: F,
    t0: time::Instant,
    metrics: ResponseMetrics<L, H>,
}

// === impl ResponseMetricsFamilies ===

impl<L, H> Default for ResponseMetricsFamilies<L, H>
where
    L: EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone,
    H: Default,
{
    fn default() -> Self {
        Self {
            request_durations: Family::new_with_constructor(H::default()),
        }
    }
}

impl<L, H> ResponseMetricsFamilies<L, H>
where
    L: EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone + Send + Sync + 'static,
    H: MetricConstructor<Histogram> + Clone + Send + Sync + 'static,
{
    pub fn register(registry: &mut Registry, mk_histo: H) -> Self
where {
        let request_durations = RequestDurationsFamily::new_with_constructor(mk_histo);
        registry.register_with_unit(
            "request_duration",
            "The durations between sending an HTTP request and receiving response headers",
            Unit::Seconds,
            request_durations.clone(),
        );

        Self { request_durations }
    }

    pub fn metrics(&self, labels: &L) -> ResponseMetrics<L, H> {
        ResponseMetrics {
            labels: labels.clone(),
            request_durations: self.request_durations.clone(),
        }
    }
}

// === impl NewCountResponses ===

impl<X: Clone, L, H, N> NewResponseMetrics<X, L, H, N> {
    pub fn new(extract: X, inner: N) -> Self {
        Self {
            extract,
            inner,
            _marker: std::marker::PhantomData,
        }
    }

    pub fn layer_via(extract: X) -> impl svc::layer::Layer<N, Service = Self> + Clone {
        svc::layer::mk(move |inner| Self::new(extract.clone(), inner))
    }
}

impl<T, X, L, H, N> svc::NewService<T> for NewResponseMetrics<X, L, H, N>
where
    X: svc::ExtractParam<ResponseMetrics<L, H>, T>,
    L: EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone + Send + Sync + 'static,
    N: svc::NewService<T>,
{
    type Service = ResponseMetricsService<L, H, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let rc = self.extract.extract_param(&target);
        let inner = self.inner.new_service(target);
        ResponseMetricsService::new(rc, inner)
    }
}

// === impl CountResponses ===

impl<L, H, S> ResponseMetricsService<L, H, S> {
    pub(crate) fn new(metrics: ResponseMetrics<L, H>, inner: S) -> Self {
        Self { metrics, inner }
    }
}

impl<B, L, H, RspB, S> svc::Service<http::Request<B>> for ResponseMetricsService<L, H, S>
where
    L: EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone + Send + Sync + 'static,
    H: MetricConstructor<Histogram> + Clone + Send + Sync + 'static,
    S: svc::Service<http::Request<B>, Response = http::Response<RspB>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = ResponseFuture<L, H, S::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        ResponseFuture {
            t0: time::Instant::now(),
            metrics: self.metrics.clone(),
            future: self.inner.call(req),
        }
    }
}

// === impl Labels ===

impl<L> prometheus_client::encoding::EncodeLabelSet for Labels<L>
where
    L: EncodeLabelSet,
{
    fn encode(
        &self,
        mut enc: prometheus_client::encoding::LabelSetEncoder<'_>,
    ) -> std::fmt::Result {
        use prometheus_client::encoding::EncodeLabel;
        ("status_code", self.status_code).encode(enc.encode_label())?;
        self.common.encode(enc)?;
        Ok(())
    }
}

// === impl ResponseFuture ===

impl<B, E, L, H, F> Future for ResponseFuture<L, H, F>
where
    L: EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone + Send + Sync + 'static,
    H: MetricConstructor<Histogram> + Clone + Send + Sync + 'static,
    F: Future<Output = Result<http::Response<B>, E>>,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let res = futures::ready!(this.future.poll(cx));
        let status_code = res.as_ref().map(|rsp| rsp.status().as_u16()).unwrap_or(0);
        let labels = Labels {
            status_code,
            common: this.metrics.labels.clone(),
        };
        let elapsed = time::Instant::now().saturating_duration_since(*this.t0);
        this.metrics
            .request_durations
            .get_or_create(&labels)
            .observe(elapsed.as_secs_f64());
        Poll::Ready(res)
    }
}
