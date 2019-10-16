use super::metric_labels::Direction;
use crate::proxy::http::metrics::handle_time;
use linkerd2_metrics::{FmtMetrics, Metric};
use std::{fmt, iter};

#[derive(Clone, Debug)]
pub struct Metrics {
    inbound: handle_time::Scope,
    outbound: handle_time::Scope,
}

impl Metrics {
    pub const HELP: &'static str =
        "A histogram of the time in microseconds between when a request is received and when it is sent upstream.";
    pub const NAME: &'static str = "request_handle_us";

    pub fn new() -> Self {
        Self {
            inbound: handle_time::Scope::new(),
            outbound: handle_time::Scope::new(),
        }
    }

    pub fn outbound(&self) -> handle_time::Scope {
        self.outbound.clone()
    }

    pub fn inbound(&self) -> handle_time::Scope {
        self.inbound.clone()
    }

    fn metric(&self) -> Metric<'_, handle_time::Scope> {
        Metric::new(Self::NAME, Self::HELP)
    }

    fn scopes<'a>(&'a self) -> impl Iterator<Item = (Direction, &'a handle_time::Scope)> {
        iter::once((Direction::In, &self.inbound))
            .chain(iter::once((Direction::Out, &self.outbound)))
    }
}

impl FmtMetrics for Metrics {
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let metric = self.metric();
        metric.fmt_help(f)?;
        metric.fmt_scopes(f, self.scopes(), |s| s)
    }
}
