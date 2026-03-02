use self::{
    count_reqs::*, labels::RouteLabels, req_body::*, req_duration::*, rsp_body::*, rsp_duration::*,
    status::*,
};
use crate::InboundMetrics;
use linkerd_app_core::svc;

pub mod count_reqs;
pub mod labels;
pub mod req_body;
pub mod req_duration;
pub mod rsp_body;
pub mod rsp_duration;
pub mod status;

pub(super) fn layer<N>(
    InboundMetrics {
        request_count,
        request_body_data,
        request_duration,
        response_body_data,
        response_duration,
        status_codes,
        ..
    }: &InboundMetrics,
) -> impl svc::Layer<N, Service = Instrumented<N>> {
    use svc::Layer as _;

    let count = {
        let extract = ExtractRequestCount(request_count.clone());
        count_reqs::NewCountRequests::layer_via(extract)
    };

    let response_duration = {
        let extract = ExtractResponseDurationMetrics(response_duration.clone());
        NewResponseDuration::layer_via(extract)
    };

    let request_duration = {
        let extract = ExtractRequestDurationMetrics(request_duration.clone());
        NewRequestDuration::layer_via(extract)
    };

    let response_body = {
        let extract = ExtractResponseBodyDataMetrics::new(response_body_data.clone());
        NewRecordResponseBodyData::layer_via(extract)
    };

    let request_body = {
        let extract = ExtractRequestBodyDataParams::new(request_body_data.clone());
        NewRecordRequestBodyData::layer_via(extract)
    };

    let status = {
        let extract = ExtractStatusCodeParams::new(status_codes.clone());
        NewRecordStatusCode::layer_via(extract)
    };

    svc::layer::mk(move |inner| {
        count.layer(response_duration.layer(
            request_duration.layer(response_body.layer(request_body.layer(status.layer(inner)))),
        ))
    })
}

/// An `N`-typed service instrumented with metrics middleware.
type Instrumented<N> = NewCountRequests<
    NewResponseDuration<
        NewRequestDuration<
            NewRecordResponseBodyData<NewRecordRequestBodyData<NewRecordStatusCode<N>>>,
        >,
    >,
>;
