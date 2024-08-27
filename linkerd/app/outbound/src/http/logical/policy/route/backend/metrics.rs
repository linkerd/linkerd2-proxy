#![allow(warnings)]

use crate::{BackendRef, ParentRef, RouteRef};
use futures::Stream;
use linkerd_app_core::{
    metrics::prom::{self, encoding::*, EncodeLabelSetMut},
    svc,
};
use linkerd_http_prom::{
    record_response::{self, NewResponseDuration},
    NewCountRequests, RequestCount, RequestCountFamilies,
};

pub use super::super::metrics::*;

#[cfg(feature = "FIXME")]
#[cfg(test)]
mod tests;

#[derive(Debug)]
pub struct RouteBackendMetrics {
    requests: RequestCountFamilies<labels::RouteBackend>,
    responses: ResponseMetrics,
}

type ResponseMetrics = ();

#[derive(Clone, Debug)]
pub struct ExtractRequestCount(RequestCountFamilies<labels::RouteBackend>);

// === impl RouteBackendMetrics ===

impl RouteBackendMetrics {
    pub fn register(reg: &mut prom::Registry, histo: impl IntoIterator<Item = f64>) -> Self {
        let requests = RequestCountFamilies::register(reg);
        // let responses = record_response::ResponseMetrics::register(reg, histo);
        Self {
            requests,
            responses: (),
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

    // #[cfg(test)]
    // pub(crate) fn get_statuses(&self, l: &L::StatusLabels) -> prom::Counter {
    //     self.responses.get_statuses(l)
    // }
}

impl Default for RouteBackendMetrics {
    fn default() -> Self {
        Self {
            requests: Default::default(),
            responses: Default::default(),
        }
    }
}

impl Clone for RouteBackendMetrics {
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
