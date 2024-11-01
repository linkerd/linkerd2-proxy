use crate::{BackendRef, ParentRef, RouteRef};
use linkerd_app_core::{metrics::prom, svc};
use linkerd_http_prom::{
    body_data::response::{BodyDataMetrics, NewRecordBodyData, ResponseBodyFamilies},
    record_response::{self, NewResponseDuration, StreamLabel},
    NewCountRequests, RequestCount, RequestCountFamilies,
};

pub use super::super::metrics::*;
pub use linkerd_http_prom::record_response::MkStreamLabel;

#[cfg(test)]
mod tests;

#[derive(Debug)]
pub struct RouteBackendMetrics<L: StreamLabel> {
    requests: RequestCountFamilies<labels::RouteBackend>,
    responses: ResponseMetrics<L>,
    body_metrics: ResponseBodyFamilies<labels::RouteBackend>,
}

type ResponseMetrics<L> = record_response::ResponseMetrics<
    <L as StreamLabel>::DurationLabels,
    <L as StreamLabel>::StatusLabels,
>;

type MetricsLayer<T, N> = BodyDataLayer<RequestLayer<ResponseLayer<T, N>>>;
type BodyDataLayer<N> = NewRecordBodyData<ExtractRecordBodyDataParams, N>;
type RequestLayer<N> = NewCountRequests<ExtractRequestCount, N>;
type ResponseLayer<T, N> = NewResponseDuration<
    T,
    ExtractRecordDurationParams<ResponseMetrics<<T as MkStreamLabel>::StreamLabel>>,
    N,
>;

pub fn layer<T, N>(
    metrics: &RouteBackendMetrics<T::StreamLabel>,
) -> impl svc::Layer<N, Service = MetricsLayer<T, N>> + Clone
where
    T: MkStreamLabel,
{
    let RouteBackendMetrics {
        requests,
        responses,
        body_metrics,
    } = metrics.clone();

    let requests = NewCountRequests::layer_via(ExtractRequestCount(requests.clone()));
    let responses = NewRecordDuration::layer_via(ExtractRecordDurationParams(responses.clone()));
    let body_metrics =
        NewRecordBodyData::layer_via(ExtractRecordBodyDataParams(body_metrics.clone()));

    svc::layers()
        .push(responses)
        .push(requests)
        .push(body_metrics)
}

#[derive(Clone, Debug)]
pub struct ExtractRequestCount(RequestCountFamilies<labels::RouteBackend>);

#[derive(Clone, Debug)]
pub struct ExtractRecordBodyDataParams(ResponseBodyFamilies<labels::RouteBackend>);

// === impl RouteBackendMetrics ===

impl<L: StreamLabel> RouteBackendMetrics<L> {
    pub fn register(reg: &mut prom::Registry, histo: impl IntoIterator<Item = f64>) -> Self {
        let requests = RequestCountFamilies::register(reg);
        let responses = record_response::ResponseMetrics::register(reg, histo);
        let body_metrics = ResponseBodyFamilies::register(reg);
        Self {
            requests,
            responses,
            body_metrics,
        }
    }

    #[cfg(test)]
    pub(crate) fn backend_request_count(
        &self,
        p: ParentRef,
        r: RouteRef,
        b: BackendRef,
    ) -> linkerd_http_prom::RequestCount {
        self.requests.metrics(&labels::RouteBackend(p, r, b))
    }

    #[cfg(test)]
    pub(crate) fn get_statuses(&self, l: &L::StatusLabels) -> prom::Counter {
        self.responses.get_statuses(l)
    }

    #[cfg(test)]
    pub(crate) fn get_response_body_metrics(
        &self,
        l: &labels::RouteBackend,
    ) -> linkerd_http_prom::body_data::response::BodyDataMetrics {
        self.body_metrics.get(l)
    }
}

impl<L: StreamLabel> Default for RouteBackendMetrics<L> {
    fn default() -> Self {
        Self {
            requests: Default::default(),
            responses: Default::default(),
            body_metrics: Default::default(),
        }
    }
}

impl<L: StreamLabel> Clone for RouteBackendMetrics<L> {
    fn clone(&self) -> Self {
        Self {
            requests: self.requests.clone(),
            responses: self.responses.clone(),
            body_metrics: self.body_metrics.clone(),
        }
    }
}

// === impl ExtractRequestCount ===

impl<T> svc::ExtractParam<RequestCount, T> for ExtractRequestCount
where
    T: svc::Param<ParentRef> + svc::Param<RouteRef> + svc::Param<BackendRef>,
{
    fn extract_param(&self, t: &T) -> RequestCount {
        self.0
            .metrics(&labels::RouteBackend(t.param(), t.param(), t.param()))
    }
}

// === impl ExtractRecordBodyDataParams ===

impl<T> svc::ExtractParam<BodyDataMetrics, T> for ExtractRecordBodyDataParams
where
    T: svc::Param<ParentRef> + svc::Param<RouteRef> + svc::Param<BackendRef>,
{
    fn extract_param(&self, t: &T) -> BodyDataMetrics {
        let Self(families) = self;
        let labels = labels::RouteBackend(t.param(), t.param(), t.param());

        families.get(&labels)
    }
}
