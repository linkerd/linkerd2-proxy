pub(crate) mod error;

pub use linkerd_app_core::metrics::*;

#[derive(Clone, Debug)]
pub struct Metrics {
    pub(crate) http_errors: error::Http,
    pub(crate) tcp_errors: error::Tcp,
    pub(crate) proxy: Proxy,
}

impl Metrics {
    pub fn new(proxy: Proxy) -> Self {
        Self {
            http_errors: error::Http::default(),
            tcp_errors: error::Tcp::default(),
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
