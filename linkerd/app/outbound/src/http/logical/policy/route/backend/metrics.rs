#![allow(warnings)]

use crate::{BackendRef, ParentRef, RouteRef};
use linkerd_app_core::{
    metrics::prom::{self, encoding::*, EncodeLabelSetMut},
    // svc,
};
use linkerd_http_prom::record_response;
// use linkerd_http_prom::record_response::ResponseDuration;
// use linkerd_http_prom::HttpMetricsFamiles;

pub use super::super::metrics::*;
pub use linkerd_http_prom::record_response::MkStreamLabel;

// pub type BackendHttpMetrics =
//     linkerd_http_prom::HttpMetrics<RouteBackend, RequestDurationHistogram>;

// pub type NewBackendHttpMetrics<N> = linkerd_http_prom::NewHttpMetrics<
//     RouteBackendMetrics,
//     RouteBackend,
//     RequestDurationHistogram,
//     N,
// >;

#[derive(Clone, Debug)]
pub struct RouteBackendMetrics<DurL, TotL> {
    // metrics: HttpMetricsFamiles<RouteBackend, RequestDurationHistogram>,
    pub(super) responses: record_response::ResponseMetrics<DurL, TotL>,
}

pub type Metrics<T> =
    RouteBackendMetrics<<T as MkStreamLabel>::DurationLabels, <T as MkStreamLabel>::TotalLabels>;

// === impl RouteBackendMetrics ===

impl<RspL> RouteBackendMetrics<labels::RouteBackend, labels::RouteBackendRsp<RspL>>
where
    RspL:
        EncodeLabelSetMut + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
{
    pub fn register(reg: &mut prom::Registry) -> Self {
        let responses = record_response::ResponseMetrics::register(reg);
        Self { responses }
    }

    // #[cfg(test)]
    // pub(crate) fn get(&self, p: ParentRef, r: RouteRef, b: BackendRef) -> BackendHttpMetrics {
    //     self.metrics.metrics(&RouteBackend(p, r, b))
    // }
}

// impl<T> svc::ExtractParam<BackendHttpMetrics, T> for RouteBackendMetrics
// where
//     T: svc::Param<ParentRef> + svc::Param<RouteRef> + svc::Param<BackendRef>,
// {
//     fn extract_param(&self, t: &T) -> BackendHttpMetrics {
//         self.metrics
//             .metrics(&RouteBackend(t.param(), t.param(), t.param()))
//     }
// }

impl<RspL> Default for RouteBackendMetrics<labels::RouteBackend, labels::RouteBackendRsp<RspL>>
where
    RspL:
        EncodeLabelSetMut + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
{
    fn default() -> Self {
        Self {
            // metrics: Default::default(),
            responses: Default::default(),
        }
    }
}
