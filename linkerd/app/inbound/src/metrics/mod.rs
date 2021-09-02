//! Inbound proxy metrics.
//!
//! While this module is very similar to `outbound::metrics`, it is bound to `inbound_`-prefixed
//! metrics and derives its labels from inbound-specific types. Eventually, we won't rely on the
//! legacy `proxy` metrics and all inbound metrics will be defined in this module.
//!
//! TODO(ver) We use a `RwLock` to store our error metrics because we don't expect these registries
//! to be updated frequently or in a performance-critical area. We should probably look to use
//! `DashMap` as we migrate our metrics registries.

pub(crate) mod authz;
pub(crate) mod error;

pub use linkerd_app_core::metrics::*;

/// Holds outbound proxy metrics.
#[derive(Clone, Debug)]
pub struct Metrics {
    pub(crate) http_authz: authz::HttpAuthzMetrics,
    pub http_errors: error::HttpErrorMetrics,

    pub(crate) tcp_authz: authz::TcpAuthzMetrics,
    pub tcp_errors: error::TcpErrorMetrics,

    /// Holds metrics that are common to both inbound and outbound proxies. These metrics are
    /// reported separately
    pub proxy: Proxy,
}

impl Metrics {
    pub fn new(proxy: Proxy) -> Self {
        Self {
            http_authz: authz::HttpAuthzMetrics::default(),
            http_errors: error::HttpErrorMetrics::default(),
            tcp_authz: authz::TcpAuthzMetrics::default(),
            tcp_errors: error::TcpErrorMetrics::default(),
            proxy,
        }
    }
}

impl FmtMetrics for Metrics {
    fn fmt_metrics(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.http_errors.fmt_metrics(f)?;
        self.tcp_errors.fmt_metrics(f)?;

        // XXX: Proxy metrics are reported elsewhere.

        Ok(())
    }
}
