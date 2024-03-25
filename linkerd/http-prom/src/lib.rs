#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use linkerd_stack::{layer, ExtractParam, NewService};
use prometheus_client::encoding::EncodeLabelSet;

mod count_reqs;
mod count_rsps;

pub use self::{
    count_reqs::{CountRequests, NewCountRequests, RequestCount, RequestCountFamilies},
    count_rsps::{
        NewResponseMetrics, ResponseMetrics, ResponseMetricsFamilies, ResponseMetricsService,
    },
};

#[derive(Clone, Debug)]
pub struct HttpMetricsFamiles<L> {
    request: RequestCountFamilies<L>,
    response: ResponseMetricsFamilies<L>,
}

#[derive(Clone, Debug)]
pub struct HttpMetrics<L> {
    request: RequestCount,
    response: ResponseMetrics<L>,
}

// === NewHttpMetrics ===

#[derive(Clone, Debug)]
pub struct NewHttpMetrics<X, L, N> {
    inner: N,
    extract: X,
    _marker: std::marker::PhantomData<fn() -> L>,
}

impl<X: Clone, L, N> NewHttpMetrics<X, L, N> {
    pub fn layer_via(extract: X) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            extract: extract.clone(),
            _marker: std::marker::PhantomData,
        })
    }
}

impl<T, X, L, N> NewService<T> for NewHttpMetrics<X, L, N>
where
    X: ExtractParam<HttpMetrics<L>, T>,
    X: Clone + Send + Sync + 'static,
    L: EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone + Send + Sync + 'static,
    N: NewService<T>,
{
    type Service = CountRequests<ResponseMetricsService<L, N::Service>>;

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

impl<L> Default for HttpMetricsFamiles<L>
where
    L: EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone,
{
    fn default() -> Self {
        Self {
            request: RequestCountFamilies::default(),
            response: ResponseMetricsFamilies::default(),
        }
    }
}

impl<L> HttpMetricsFamiles<L>
where
    L: EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone + Send + Sync + 'static,
{
    pub fn register(reg: &mut prometheus_client::registry::Registry) -> Self {
        let request = RequestCountFamilies::register(reg);
        let response = ResponseMetricsFamilies::register(reg);
        Self { request, response }
    }

    pub fn metrics(&self, labels: &L) -> HttpMetrics<L> {
        HttpMetrics {
            request: self.request.metrics(labels),
            response: self.response.metrics(labels),
        }
    }
}

// === HttpMetrics ===

impl<L> HttpMetrics<L> {
    pub fn requests_total(&self) -> &RequestCount {
        &self.request
    }

    pub fn response(&self) -> &ResponseMetrics<L> {
        &self.response
    }
}
