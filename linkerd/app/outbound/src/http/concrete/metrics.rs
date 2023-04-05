use crate::{BackendRef, ParentRef};
use ahash::AHashMap;
use linkerd_app_core::{
    metrics::{metrics, FmtLabels, FmtMetrics, Gauge},
    svc::http::balance,
};
use parking_lot::Mutex;
use std::{fmt::Write, sync::Arc};

metrics! {
    outbound_http_balancer_endpoints: Gauge {
        "The number of endpoints currently in a HTTP request balancer"
    }
}

#[derive(Clone, Debug, Default)]
pub struct BalancerMetrics {
    balancers: Arc<Mutex<AHashMap<Labels, balance::EndpointsGauges>>>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct Labels(ParentRef, BackendRef);

struct Ready<'l>(&'l Labels);
struct Pending<'l>(&'l Labels);

// === impl RouteBackendMetrics ===

impl BalancerMetrics {
    pub(super) fn http_endpoints(&self, pr: ParentRef, br: BackendRef) -> balance::EndpointsGauges {
        self.balancers
            .lock()
            .entry(Labels(pr, br))
            .or_default()
            .clone()
    }
}

impl FmtMetrics for BalancerMetrics {
    fn fmt_metrics(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let balancers = self.balancers.lock();
        if !balancers.is_empty() {
            outbound_http_balancer_endpoints.fmt_help(f)?;
            outbound_http_balancer_endpoints.fmt_scopes(
                f,
                balancers.iter().map(|(l, e)| (Pending(l), e)),
                |c| &c.pending,
            )?;
            outbound_http_balancer_endpoints.fmt_scopes(
                f,
                balancers.iter().map(|(l, e)| (Ready(l), e)),
                |c| &c.ready,
            )?;
        }
        drop(balancers);

        Ok(())
    }
}

impl FmtLabels for Labels {
    fn fmt_labels(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Labels(parent, backend) = self;

        crate::metrics::write_service_meta_labels("parent", parent, f)?;
        f.write_char(',')?;
        crate::metrics::write_service_meta_labels("backend", backend, f)?;

        Ok(())
    }
}

impl<'l> FmtLabels for Ready<'l> {
    fn fmt_labels(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt_labels(f)?;
        write!(f, ",endpoint_state=\"ready\"")
    }
}

impl<'l> FmtLabels for Pending<'l> {
    fn fmt_labels(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt_labels(f)?;
        write!(f, ",endpoint_state=\"pending\"")
    }
}
