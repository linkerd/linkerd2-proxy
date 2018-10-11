use std::fmt;
use std::hash::Hash;

use metrics::{FmtLabels, FmtMetrics};
use proxy;
use transport::metrics as transport;
use super::{process, tls_config_reload};

/// Implements `FmtMetrics` to report runtime metrics.
#[derive(Clone, Debug)]
pub struct Report<T, C>
where
    T: FmtLabels + Hash + Eq,
    C: FmtLabels + Hash + Eq,
{
    http: proxy::http::metrics::Report<T, C>,
    transports: transport::Report,
    tls_config_reload: tls_config_reload::Report,
    process: process::Report,
}

// ===== impl Report =====

impl<T, C> Report<T, C>
where
    T: FmtLabels + Hash + Eq,
    C: FmtLabels + Hash + Eq,
{
    pub fn new(
        http: proxy::http::metrics::Report<T, C>,
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

impl<T, C> FmtMetrics for Report<T, C>
where
    T: FmtLabels + Hash + Eq,
    C: FmtLabels + Hash + Eq,
{
    fn fmt_metrics(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.http.fmt_metrics(f)?;
        self.transports.fmt_metrics(f)?;
        self.tls_config_reload.fmt_metrics(f)?;
        self.process.fmt_metrics(f)?;

        Ok(())
    }
}
