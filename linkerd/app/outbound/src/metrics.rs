//! Outbound proxy metrics.
//!
//! While this module is very similar to `inbound::metrics`, it is bound to `outbound_`-prefixed
//! metrics and derives its labels from outbound-specific types. Eventually, we won't rely on the
//! legacy `proxy` metrics and all outbound metrics will be defined in this module.
//!
//! TODO(ver) We use a `RwLock` to store our error metrics because we don't expect these registries
//! to be updated frequently or in a performance-critical area. We should probably look to use
//! `DashMap` as we migrate other metrics registries.

use crate::http::policy::RouteBackendMetrics;

pub(crate) mod error;

pub use linkerd_app_core::metrics::*;

/// Holds outbound proxy metrics.
#[derive(Clone, Debug)]
pub struct OutboundMetrics {
    pub(crate) http_errors: error::Http,
    pub(crate) tcp_errors: error::Tcp,

    pub(crate) http_route_backends: RouteBackendMetrics,

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
