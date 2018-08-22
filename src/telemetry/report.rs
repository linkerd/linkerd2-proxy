use std::fmt;

use super::{http, process, tls_config_reload, transport};
use super::metrics::FmtMetrics;

/// Implements `FmtMetrics` to report runtime metrics.
#[derive(Clone, Debug)]
pub struct Report {
    http: http::Report,
    transports: transport::Report,
    tls_config_reload: tls_config_reload::Report,
    process: process::Report,
}

// ===== impl Report =====

impl Report {
    pub(super) fn new(
        http: http::Report,
        transports: transport::Report,
        tls_config_reload: tls_config_reload::Report,
        process: process::Report,
    ) -> Self {
        Self {
            http,
            transports,
            tls_config_reload,
            process,
        }
    }
}

// ===== impl Report =====

impl FmtMetrics for Report {
    fn fmt_metrics(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.http.fmt_metrics(f)?;
        self.transports.fmt_metrics(f)?;
        self.tls_config_reload.fmt_metrics(f)?;
        self.process.fmt_metrics(f)?;

        Ok(())
    }
}
