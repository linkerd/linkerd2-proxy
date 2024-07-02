use crate::{BackendRef, ParentRef, RouteRef};
use linkerd_app_core::{
    metrics::prom::{self, encoding::*, EncodeLabelSetMut, Histogram},
    svc,
};
use linkerd_http_prom::HttpMetricsFamiles;
use prometheus_client::metrics::family::MetricConstructor;

pub type BackendHttpMetrics =
    linkerd_http_prom::HttpMetrics<RouteBackendLabels, RequestDurationHistogram>;

pub type NewBackendHttpMetrics<N> = linkerd_http_prom::NewHttpMetrics<
    RouteBackendMetrics,
    RouteBackendLabels,
    RequestDurationHistogram,
    N,
>;

#[derive(Clone, Debug, Default)]
pub struct RouteBackendMetrics {
    metrics: HttpMetricsFamiles<RouteBackendLabels, RequestDurationHistogram>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RouteBackendLabels(ParentRef, RouteRef, BackendRef);

#[derive(Clone, Debug, Default)]
pub struct RequestDurationHistogram;

// === impl RouteBackendMetrics ===

impl RouteBackendMetrics {
    pub fn register(reg: &mut prom::Registry) -> Self {
        Self {
            metrics: HttpMetricsFamiles::register(reg, RequestDurationHistogram),
        }
    }

    #[cfg(test)]
    pub(crate) fn get(&self, p: ParentRef, r: RouteRef, b: BackendRef) -> BackendHttpMetrics {
        self.metrics.metrics(&RouteBackendLabels(p, r, b))
    }
}

impl<T> svc::ExtractParam<BackendHttpMetrics, T> for RouteBackendMetrics
where
    T: svc::Param<ParentRef> + svc::Param<RouteRef> + svc::Param<BackendRef>,
{
    fn extract_param(&self, t: &T) -> BackendHttpMetrics {
        self.metrics
            .metrics(&RouteBackendLabels(t.param(), t.param(), t.param()))
    }
}

// === impl RouteBackendLabels ===

impl EncodeLabelSetMut for RouteBackendLabels {
    fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        let Self(parent, route, backend) = self;
        parent.encode_label_set(enc)?;
        route.encode_label_set(enc)?;
        backend.encode_label_set(enc)?;
        Ok(())
    }
}

impl EncodeLabelSet for RouteBackendLabels {
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}

// === impl RequestDurationHistogram ===

impl RequestDurationHistogram {
    const BUCKETS: &'static [f64] = &[0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0];
}

impl MetricConstructor<Histogram> for RequestDurationHistogram {
    fn new_metric(&self) -> Histogram {
        Histogram::new(Self::BUCKETS.iter().copied())
    }
}
