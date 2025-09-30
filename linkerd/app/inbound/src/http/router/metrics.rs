use crate::InboundMetrics;
use linkerd_app_core::svc;

pub use self::{count_reqs::*, labels::RouteLabels, req_body::*, rsp_body::*};

mod count_reqs;
mod labels;
mod req_body;
mod rsp_body;

pub(super) fn layer<N>(
    InboundMetrics {
        request_count,
        request_body_data,
        response_body_data,
        ..
    }: &InboundMetrics,
) -> impl svc::Layer<N, Service = Instrumented<N>> {
    use svc::Layer as _;

    let count = {
        let extract = ExtractRequestCount(request_count.clone());
        count_reqs::NewCountRequests::layer_via(extract)
    };

    let body = {
        let extract = ExtractResponseBodyDataMetrics::new(response_body_data.clone());
        NewRecordResponseBodyData::layer_via(extract)
    };

    let request = {
        let extract = ExtractRequestBodyDataParams::new(request_body_data.clone());
        NewRecordRequestBodyData::new(extract)
    };

    svc::layer::mk(move |inner| count.layer(body.layer(request.layer(inner))))
}

/// An `N`-typed service instrumented with metrics middleware.
type Instrumented<N> = NewCountRequests<NewRecordResponseBodyData<NewRecordRequestBodyData<N>>>;
