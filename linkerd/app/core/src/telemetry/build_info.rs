use linkerd_metrics::{metrics, FmtLabels, FmtMetric, FmtMetrics, Gauge};
use std::{env, fmt};

pub const VERSION: &str = env!("LINKERD2_PROXY_VERSION");
pub const DATE: &str = env!("LINKERD2_PROXY_BUILD_DATE");
pub const VENDOR: &str = env!("LINKERD2_PROXY_VENDOR");
pub const GIT_SHA: &str = env!("GIT_SHA");
pub const PROFILE: &str = env!("PROFILE");

metrics! {
    proxy_build_info: Gauge {
        "Proxy build info"
    }
}

#[derive(Clone, Debug, Default)]
pub struct Report(());

struct Labels;

impl FmtMetrics for Report {
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        proxy_build_info.fmt_help(f)?;
        Gauge::from(1).fmt_metric_labeled(f, "proxy_build_info", &Labels)?;
        Ok(())
    }
}

impl FmtLabels for Labels {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "version=\"{VERSION}\"")?;
        write!(f, ",git_sha=\"{GIT_SHA}\"")?;
        write!(f, ",profile=\"{PROFILE}\"")?;
        write!(f, ",date=\"{DATE}\"")?;
        write!(f, ",vendor=\"{VENDOR}\"")?;
        Ok(())
    }
}
