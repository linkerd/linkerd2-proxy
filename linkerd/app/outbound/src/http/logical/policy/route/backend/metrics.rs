use crate::{BackendRef, ParentRef, RouteRef};
use linkerd_app_core::{metrics::prom, svc};
use linkerd_http_prom::{
    record_response::{self, MkStreamLabel, NewResponseDuration, StreamLabel},
    NewCountRequests, RequestCount, RequestCountFamilies,
};

pub use super::super::metrics::*;

#[cfg(test)]
mod tests;

#[derive(Debug)]
pub struct RouteBackendMetrics<L: StreamLabel> {
    requests: RequestCountFamilies<labels::RouteBackend>,
    responses: ResponseMetrics<L>,
}

type ResponseMetrics<L> = record_response::ResponseMetrics<
    <L as StreamLabel>::DurationLabels,
    <L as StreamLabel>::StatusLabels,
>;

pub fn layer<T, N>(
    metrics: &RouteBackendMetrics<T::StreamLabel>,
) -> impl svc::Layer<
    N,
    Service = NewCountRequests<
        ExtractRequestCount,
        NewResponseDuration<T, ExtractRecordDurationParams<ResponseMetrics<T::StreamLabel>>, N>,
    >,
> + Clone
where
    T: MkStreamLabel,
    N: svc::NewService<T>,
    NewCountRequests<
        ExtractRequestCount,
        NewResponseDuration<T, ExtractRecordDurationParams<ResponseMetrics<T::StreamLabel>>, N>,
    >: svc::NewService<T>,
    NewResponseDuration<T, ExtractRecordDurationParams<ResponseMetrics<T::StreamLabel>>, N>:
        svc::NewService<T>,
{
    let RouteBackendMetrics {
        requests,
        responses,
    } = metrics.clone();
    svc::layer::mk(move |inner| {
        use svc::Layer;
        NewCountRequests::layer_via(ExtractRequestCount(requests.clone())).layer(
            NewRecordDuration::layer_via(ExtractRecordDurationParams(responses.clone()))
                .layer(inner),
        )
    })
}

#[derive(Clone, Debug)]
pub struct ExtractRequestCount(RequestCountFamilies<labels::RouteBackend>);

// === impl RouteBackendMetrics ===

impl<L: StreamLabel> RouteBackendMetrics<L> {
    pub fn register(reg: &mut prom::Registry, histo: impl IntoIterator<Item = f64>) -> Self {
        let requests = RequestCountFamilies::register(reg);
        let responses = record_response::ResponseMetrics::register(reg, histo);
        Self {
            requests,
            responses,
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
}

impl<L: StreamLabel> Default for RouteBackendMetrics<L> {
    fn default() -> Self {
        Self {
            requests: Default::default(),
            responses: Default::default(),
        }
    }
}

impl<L: StreamLabel> Clone for RouteBackendMetrics<L> {
    fn clone(&self) -> Self {
        Self {
            requests: self.requests.clone(),
            responses: self.responses.clone(),
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
