#![allow(warnings)]

use crate::{BackendRef, ParentRef, RouteRef};
use futures::Stream;
use linkerd_app_core::{
    metrics::prom::{self, encoding::*, EncodeLabelSetMut},
    // svc,
};
use linkerd_http_prom::{
    record_response::{self, StreamLabel},
    RequestCountFamilies,
};

pub use super::super::metrics::*;
pub use linkerd_http_prom::record_response::MkStreamLabel;

#[derive(Debug)]
pub struct RouteBackendMetrics<L: StreamLabel> {
    requests: RequestCountFamilies<labels::RouteBackend>,
    pub(super) responses:
        record_response::ResponseMetrics<L::AggregateLabels, L::DetailedSummaryLabels>,
}

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

    // #[cfg(test)]
    // pub(crate) fn get(&self, p: ParentRef, r: RouteRef, b: BackendRef) -> BackendHttpMetrics {
    //     self.metrics.metrics(&RouteBackend(p, r, b))
    // }
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
            requests: Default::default(),
            responses: self.responses.clone(),
        }
    }
}
