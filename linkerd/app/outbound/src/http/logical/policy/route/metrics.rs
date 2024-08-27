#![allow(warnings)]

use super::{backend::metrics as backend, retry};
use linkerd_app_core::metrics::prom;

pub mod labels;
#[cfg(test)]
pub(super) mod test_util;

#[cfg(feature = "FIXME")]
#[cfg(test)]
mod tests;

pub type RequestMetrics = ();

#[derive(Debug)]
pub struct RouteMetrics {
    pub(super) retry: retry::RouteRetryMetrics,
    pub(super) requests: RequestMetrics,
    pub(super) backend: backend::RouteBackendMetrics,
}

// === impl RouteMetrics ===

impl RouteMetrics {
    // There are two histograms for which we need to register metrics: request
    // durations, measured on routes, and response durations, measured on
    // route-backends.
    //
    // Response duration is probably the more meaninful metric
    // operationally--and it includes more backend metadata--so we opt to
    // preserve higher fidelity for response durations (especially for lower
    // values).
    //
    // We elide several buckets for request durations to be conservative about
    // the costs of tracking these two largely overlapping histograms
    const REQUEST_BUCKETS: &'static [f64] = &[0.05, 0.5, 1.0, 10.0];
    const RESPONSE_BUCKETS: &'static [f64] = &[0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 10.0];
}

impl Default for RouteMetrics {
    fn default() -> Self {
        Self {
            requests: Default::default(),
            backend: Default::default(),
            retry: Default::default(),
        }
    }
}

impl Clone for RouteMetrics {
    fn clone(&self) -> Self {
        Self {
            requests: self.requests.clone(),
            backend: self.backend.clone(),
            retry: self.retry.clone(),
        }
    }
}

impl RouteMetrics {
    pub fn register(reg: &mut prom::Registry) -> Self {
        let requests = (); //RequestMetrics::<R>::register(reg, Self::REQUEST_BUCKETS.iter().copied());

        let backend = backend::RouteBackendMetrics::register(
            reg.sub_registry_with_prefix("backend"),
            Self::RESPONSE_BUCKETS.iter().copied(),
        );

        let retry = retry::RouteRetryMetrics::register(reg.sub_registry_with_prefix("retry"));

        Self {
            requests,
            backend,
            retry,
        }
    }

    #[cfg(test)]
    pub(crate) fn backend_request_count(
        &self,
        p: crate::ParentRef,
        r: crate::RouteRef,
        b: crate::BackendRef,
    ) -> linkerd_http_prom::RequestCount {
        self.backend.backend_request_count(p, r, b)
    }
}
