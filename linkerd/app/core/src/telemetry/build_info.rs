use linkerd2_metrics::{metrics, FmtLabels, FmtMetric, FmtMetrics, Gauge};
use std::env;
use std::fmt;
use std::string::String;
use std::sync::Arc;

const GIT_BRANCH: &'static str = env!("GIT_BRANCH");
const GIT_VERSION: &'static str = env!("GIT_VERSION");
const PROFILE: &'static str = env!("PROFILE");
const RUST_VERSION: &'static str = env!("RUST_VERSION");

metrics! {
    proxy_build_info: Gauge {
        "Proxy build info"
    }
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    name: String,
    // `value` remains constant over the lifetime of the proxy so that
    // build information in `labels` remains accurate
    value: Arc<Gauge>,
    labels: Arc<BuildInfoLabels>,
}

#[derive(Clone, Debug, Default)]
struct BuildInfoLabels {
    git_branch: String,
    git_version: String,
    profile: String,
    rust_version: String,
}

impl Report {
    pub fn new() -> Self {
        let labels = Arc::new(BuildInfoLabels {
            git_branch: GIT_BRANCH.to_string(),
            git_version: GIT_VERSION.to_string(),
            profile: PROFILE.to_string(),
            rust_version: RUST_VERSION.to_string(),
        });
        Self {
            name: "proxy_build_info".to_string(),
            value: Arc::new(1.into()),
            labels,
        }
    }
}

impl FmtMetrics for Report {
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        proxy_build_info.fmt_help(f)?;
        self.value
            .fmt_metric_labeled(f, self.name.as_str(), self.labels.as_ref())?;
        Ok(())
    }
}

impl FmtLabels for BuildInfoLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "git_branch=\"{}\"", self.git_branch)?;
        write!(f, ",git_version=\"{}\"", self.git_version)?;
        write!(f, ",profile=\"{}\"", self.profile)?;
        write!(f, ",rust_version=\"{}\"", self.rust_version)?;
        Ok(())
    }
}
