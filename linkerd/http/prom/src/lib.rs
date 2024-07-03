#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use linkerd_stack::{layer, ExtractParam, NewService};
use prometheus_client::{
    encoding::EncodeLabelSet,
    metrics::{family::MetricConstructor, histogram::Histogram},
};

mod count_reqs;
mod count_rsps;
mod request_duration;

pub use self::{
    count_reqs::{CountRequests, NewCountRequests, RequestCount, RequestCountFamilies},
    count_rsps::{
        NewResponseMetrics, ResponseMetrics, ResponseMetricsFamilies, ResponseMetricsService,
    },
    // request_duration::RequestDurationHistogram,
};

#[derive(Clone, Debug)]
pub struct HttpMetricsFamiles<L, H> {
    request: RequestCountFamilies<L>,
    response: ResponseMetricsFamilies<L, H>,
}

#[derive(Clone, Debug)]
pub struct HttpMetrics<L, H> {
    request: RequestCount,
    response: ResponseMetrics<L, H>,
}

// === NewHttpMetrics ===

#[derive(Clone, Debug)]
pub struct NewHttpMetrics<X, L, H, N> {
    inner: N,
    extract: X,
    _marker: std::marker::PhantomData<fn() -> (L, H)>,
}

impl<X: Clone, L, H, N> NewHttpMetrics<X, L, H, N> {
    pub fn layer_via(extract: X) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            extract: extract.clone(),
            _marker: std::marker::PhantomData,
        })
    }
}

impl<T, X, L, H, N> NewService<T> for NewHttpMetrics<X, L, H, N>
where
    X: ExtractParam<HttpMetrics<L, H>, T>,
    X: Clone + Send + Sync + 'static,
    L: EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone + Send + Sync + 'static,
    N: NewService<T>,
{
    type Service = CountRequests<ResponseMetricsService<L, H, N::Service>>;

    #[inline]
    fn new_service(&self, target: T) -> Self::Service {
        let HttpMetrics { request, response } = self.extract.extract_param(&target);
        CountRequests::new(
            request,
            ResponseMetricsService::new(response, self.inner.new_service(target)),
        )
    }
}

// === HttpMetricsFamilies ===

impl<L, H> Default for HttpMetricsFamiles<L, H>
where
    L: EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone,
    H: Default,
{
    fn default() -> Self {
        Self {
            request: RequestCountFamilies::default(),
            response: ResponseMetricsFamilies::default(),
        }
    }
}

impl<L, H> HttpMetricsFamiles<L, H>
where
    L: EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone + Send + Sync + 'static,
    H: MetricConstructor<Histogram> + Clone + Send + Sync + 'static,
{
    pub fn register(reg: &mut prometheus_client::registry::Registry, mk_histo: H) -> Self {
        let request = RequestCountFamilies::register(reg);
        let response = ResponseMetricsFamilies::register(reg, mk_histo);
        Self { request, response }
    }

    pub fn metrics(&self, labels: &L) -> HttpMetrics<L, H> {
        HttpMetrics {
            request: self.request.metrics(labels),
            response: self.response.metrics(labels),
        }
    }
}

// === HttpMetrics ===

impl<L, H> HttpMetrics<L, H> {
    pub fn requests_total(&self) -> &RequestCount {
        &self.request
    }

    pub fn response(&self) -> &ResponseMetrics<L, H> {
        &self.response
    }
}
