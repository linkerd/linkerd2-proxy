use crate::{metrics::ConcreteLabels, BackendRef, ParentRef};
use ahash::AHashMap;
use linkerd_app_core::{
    metrics::{metrics, FmtLabels, FmtMetrics, Gauge},
    svc::http::balance,
};
use parking_lot::Mutex;
use std::sync::Arc;

metrics! {
    outbound_http_balancer_endpoints: Gauge {
        "The number of endpoints currently in a HTTP request balancer"
    }
}

#[derive(Clone, Debug, Default)]
pub struct BalancerMetrics {
    balancers: Arc<Mutex<AHashMap<ConcreteLabels, balance::EndpointsGauges>>>,
}

struct Ready<'l>(&'l ConcreteLabels);
struct Pending<'l>(&'l ConcreteLabels);

// === impl RouteBackendMetrics ===

impl BalancerMetrics {
    pub(super) fn http_endpoints(&self, pr: ParentRef, br: BackendRef) -> balance::EndpointsGauges {
        self.balancers
            .lock()
            .entry(ConcreteLabels(pr, br))
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

// === impl Ready ===

impl<'l> FmtLabels for Ready<'l> {
    fn fmt_labels(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt_labels(f)?;
        write!(f, ",endpoint_state=\"ready\"")
    }
}

// === impl Pending ===

impl<'l> FmtLabels for Pending<'l> {
    fn fmt_labels(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt_labels(f)?;
        write!(f, ",endpoint_state=\"pending\"")
    }
}
