use linkerd_stack as svc;
use prometheus_client::{
    encoding::EncodeLabelSet,
    metrics::{counter::Counter, family::Family, histogram::Histogram},
    registry::{Registry, Unit},
};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::time;

#[derive(Clone, Debug)]
pub struct ResponseMetricsFamilies<L> {
    errors: Family<L, Counter>,
    request_durations: RequestDurationsFamily<L>,
}

type RequestDurationsFamily<L> = Family<Labels<L>, Histogram, fn() -> Histogram>;

#[derive(Clone, Debug)]
pub struct ResponseMetrics<L> {
    labels: L,
    errors: Counter,
    request_durations: Family<Labels<L>, Histogram>,
}

#[derive(Clone, Debug)]
pub struct NewResponseMetrics<X, L, N> {
    inner: N,
    extract: X,
    _marker: std::marker::PhantomData<fn() -> L>,
}

#[derive(Clone, Debug)]
pub struct ResponseMetricsService<L, S> {
    inner: S,
    metrics: ResponseMetrics<L>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Labels<L> {
    status_code: u16,
    common: L,
}

// Define the ResponseFuture type
#[pin_project::pin_project]
pub struct ResponseFuture<L, F> {
    #[pin]
    future: F,
    t0: time::Instant,
    metrics: ResponseMetrics<L>,
}

// === impl ResponseMetricsFamilies ===

impl<L> Default for ResponseMetricsFamilies<L>
where
    L: EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone,
{
    fn default() -> Self {
        Self {
            errors: Family::default(),
            request_durations: Family::new_with_constructor(|| Histogram::new(None.into_iter())),
        }
    }
}

impl<L> ResponseMetricsFamilies<L>
where
    L: EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone + Send + Sync + 'static,
{
    pub fn register(registry: &mut Registry) -> Self {
        let request_durations = RequestDurationsFamily::new_with_constructor(|| {
            Histogram::new(vec![0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0].into_iter())
        });
        registry.register_with_unit(
            "request_duration",
            "The durations between sending an HTTP request and receiving response headers",
            Unit::Seconds,
            request_durations.clone(),
        );

        let errors = Family::default();
        registry.register(
            "request_errors",
            "The total number of errors encountered while waiting for a response",
            errors.clone(),
        );

        Self {
            errors,
            request_durations,
        }
    }

    pub fn metrics(&self, labels: &L) -> ResponseMetrics<L> {
        ResponseMetrics {
            labels: labels.clone(),
            errors: self.errors.get_or_create(labels).clone(),
            request_durations: self.request_durations.clone(),
        }
    }
}

// === impl NewCountResponses ===

impl<X: Clone, L, N> NewResponseMetrics<X, L, N> {
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

impl<T, X, L, N> svc::NewService<T> for NewResponseMetrics<X, L, N>
where
    X: svc::ExtractParam<ResponseMetrics<L>, T>,
    L: EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone + Send + Sync + 'static,
    N: svc::NewService<T>,
{
    type Service = ResponseMetricsService<L, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let rc = self.extract.extract_param(&target);
        let inner = self.inner.new_service(target);
        ResponseMetricsService::new(rc, inner)
    }
}

// === impl CountResponses ===

impl<L, S> ResponseMetricsService<L, S> {
    pub(crate) fn new(metrics: ResponseMetrics<L>, inner: S) -> Self {
        Self { metrics, inner }
    }
}

impl<B, L, RspB, S> svc::Service<http::Request<B>> for ResponseMetricsService<L, S>
where
    L: EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone + Send + Sync + 'static,
    S: svc::Service<http::Request<B>, Response = http::Response<RspB>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = ResponseFuture<L, S::Future>;

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

impl<B, E, L, F> Future for ResponseFuture<L, F>
where
    L: EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone + Send + Sync + 'static,
    F: Future<Output = Result<http::Response<B>, E>>,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let res = futures::ready!(this.future.poll(cx));
        match &res {
            Ok(rsp) => {
                let labels = Labels {
                    status_code: rsp.status().as_u16(),
                    common: this.metrics.labels.clone(),
                };
                let elapsed = time::Instant::now().saturating_duration_since(*this.t0);
                this.metrics
                    .request_durations
                    .get_or_create(&labels)
                    .observe(elapsed.as_secs_f64());
            }
            Err(_) => {
                this.metrics.errors.inc();
            }
        }
        Poll::Ready(res)
    }
}
