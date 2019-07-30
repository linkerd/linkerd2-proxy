use proxy::http::metrics::handle_time;
use metrics::{Metric, FmtMetrics};
use super::metric_labels::Direction;
use std::{iter, fmt};

pub struct Scopes {
    inbound: handle_time::Scope,
    outbound: handle_time::Scope,
    metric: Metric<'static, handle_time::Scope>,
}

impl Scopes {
    pub const HELP: &'static str =
        "A histogram of the time in microseconds between when a request is received and when it is sent upstream.";
    pub const NAME: &'static str = "request_handle_us";

    pub fn new() -> Self {
        Scopes {
            inbound: handle_time::Scope::new(),
            outbound: handle_time::Scope::new(),
            metric: Metric::new(Self::NAME, Self::HELP),
        }
    }

    pub fn outbound(&self) -> handle_time::Scope {
        self.outbound.clone()
    }

    pub fn inbound(&self) -> handle_time::Scope {
        self.inbound.clone()
    }

    fn scopes<'a>(&'a self) -> impl Iterator<Item = (Direction, &'a handle_time::Scope)> {
        iter::once((Direction::In, &self.inbound))
            .chain(iter::once((Direction::Out, &self.outbound)))
    }
}


impl FmtMetrics for Scopes {
    fn fmt_metrics(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.metric.fmt_help(f)?;
        self.metric.fmt_scopes(f, self.scopes(), |s| s)
    }
}
