//! Outbound proxy metrics.
//!
//! While this module is very similar to `inbound::metrics`, it is bound to `outbound_`-prefixed
//! metrics and derives its labels from outbound-specific types. Eventually, we won't rely on the
//! legacy `proxy` metrics and all outbound metrics will be defined in this module.
//!
//! TODO(ver) We use a `RwLock` to store our error metrics because we don't expect these registries
//! to be updated frequently or in a performance-critical area. We should probably look to use
//! `DashMap` as we migrate other metrics registries.

use crate::{
    http::{concrete::BalancerMetrics, policy::RouteBackendMetrics},
    policy,
};

pub(crate) mod error;
pub use linkerd_app_core::metrics::*;

/// Holds outbound proxy metrics.
#[derive(Clone, Debug)]
pub struct OutboundMetrics {
    pub(crate) http_errors: error::Http,
    pub(crate) tcp_errors: error::Tcp,

    pub(crate) http_route_backends: RouteBackendMetrics,
    pub(crate) http_balancer: BalancerMetrics,

    /// Holds metrics that are common to both inbound and outbound proxies. These metrics are
    /// reported separately
    pub(crate) proxy: Proxy,
}

impl OutboundMetrics {
    pub(crate) fn new(proxy: Proxy) -> Self {
        Self {
            proxy,
            http_errors: error::Http::default(),
            tcp_errors: error::Tcp::default(),
            http_route_backends: RouteBackendMetrics::default(),
            http_balancer: BalancerMetrics::default(),
        }
    }
}

impl FmtMetrics for OutboundMetrics {
    fn fmt_metrics(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.http_route_backends.fmt_metrics(f)?;
        self.http_errors.fmt_metrics(f)?;
        self.tcp_errors.fmt_metrics(f)?;

        // XXX: Proxy metrics are reported elsewhere.

        Ok(())
    }
}

pub(crate) fn write_meta_labels(
    scope: &str,
    meta: &policy::Meta,
    f: &mut std::fmt::Formatter<'_>,
) -> std::fmt::Result {
    write!(f, "{scope}_group=\"{}\"", meta.group())?;
    write!(f, ",{scope}_kind=\"{}\"", meta.kind())?;
    write!(f, ",{scope}_namespace=\"{}\"", meta.namespace())?;
    write!(f, ",{scope}_name=\"{}\"", meta.name())?;
    Ok(())
}

pub(crate) fn write_service_meta_labels(
    scope: &str,
    meta: &policy::Meta,
    f: &mut std::fmt::Formatter<'_>,
) -> std::fmt::Result {
    write_meta_labels(scope, meta, f)?;
    match meta.port() {
        Some(port) => write!(f, ",{scope}_port=\"{port}\"")?,
        None => write!(f, ",{scope}_port=\"\"")?,
    }
    write!(f, ",{scope}_section_name=\"{}\"", meta.section())?;
    Ok(())
}
