#![allow(warnings)]

use crate::{BackendRef, ParentRef, RouteRef};
use linkerd_app_core::{
    metrics::prom::{self, encoding::*, EncodeLabelSetMut},
    // svc,
};
// use linkerd_http_prom::record_response::ResponseDuration;
// use linkerd_http_prom::HttpMetricsFamiles;

// pub type BackendHttpMetrics =
//     linkerd_http_prom::HttpMetrics<RouteBackendLabels, RequestDurationHistogram>;

// pub type NewBackendHttpMetrics<N> = linkerd_http_prom::NewHttpMetrics<
//     RouteBackendMetrics,
//     RouteBackendLabels,
//     RequestDurationHistogram,
//     N,
// >;

#[derive(Debug)]
pub struct RouteBackendMetrics<RspL> {
    // metrics: HttpMetricsFamiles<RouteBackendLabels, RequestDurationHistogram>,
    // response_duration: ResponseDuration<RspL>,
    _marker: std::marker::PhantomData<RspL>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RouteBackendLabels(ParentRef, RouteRef, BackendRef);

// === impl RouteBackendMetrics ===

impl<RspL> RouteBackendMetrics<RspL> {
    pub fn register(reg: &mut prom::Registry) -> Self {
        // let response_duration = ResponseDuration::register(reg);
        // Self { response_duration }
        Self {
            _marker: std::marker::PhantomData,
        }
    }

    // #[cfg(test)]
    // pub(crate) fn get(&self, p: ParentRef, r: RouteRef, b: BackendRef) -> BackendHttpMetrics {
    //     self.metrics.metrics(&RouteBackendLabels(p, r, b))
    // }
}

// impl<T> svc::ExtractParam<BackendHttpMetrics, T> for RouteBackendMetrics
// where
//     T: svc::Param<ParentRef> + svc::Param<RouteRef> + svc::Param<BackendRef>,
// {
//     fn extract_param(&self, t: &T) -> BackendHttpMetrics {
//         self.metrics
//             .metrics(&RouteBackendLabels(t.param(), t.param(), t.param()))
//     }
// }

impl<L> Clone for RouteBackendMetrics<L> {
    fn clone(&self) -> Self {
        Self {
            // metrics: self.metrics.clone(),
            // response_duration: self.response_duration.clone(),
            _marker: std::marker::PhantomData,
        }
    }
}

impl<L> Default for RouteBackendMetrics<L> {
    fn default() -> Self {
        Self {
            // metrics: Default::default(),
            // response_duration: Default::default(),
            _marker: std::marker::PhantomData,
        }
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
