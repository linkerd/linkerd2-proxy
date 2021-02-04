use linkerd_metrics::{metrics, Counter, FmtMetrics};
use std::fmt;
use std::sync::Arc;

metrics! {
    opencensus_span_export_streams: Counter { "Total count of opened span export streams" },
    opencensus_span_export_requests: Counter { "Total count of span export request messages" },
    opencensus_span_exports: Counter { "Total count of spans exported" }
}

#[derive(Debug)]
struct Metrics {
    streams: Counter,
    requests: Counter,
    spans: Counter,
}

#[derive(Clone, Debug)]
pub struct Registry(Arc<Metrics>);

#[derive(Clone, Debug)]
pub struct Report(Arc<Metrics>);

pub fn new() -> (Registry, Report) {
    let metrics = Metrics {
        streams: Counter::default(),
        requests: Counter::default(),
        spans: Counter::default(),
    };
    let shared = Arc::new(metrics);
    (Registry(shared.clone()), Report(shared))
}

impl Registry {
    pub fn start_stream(&mut self) {
        self.0.streams.incr()
    }

    pub fn send(&mut self, spans: u64) {
        self.0.requests.incr();
        self.0.spans.add(spans);
    }
}

impl FmtMetrics for Report {
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        opencensus_span_export_streams.fmt_help(f)?;
        opencensus_span_export_streams.fmt_metric(f, &self.0.streams)?;

        opencensus_span_export_requests.fmt_help(f)?;
        opencensus_span_export_requests.fmt_metric(f, &self.0.requests)?;

        opencensus_span_exports.fmt_help(f)?;
        opencensus_span_exports.fmt_metric(f, &self.0.spans)?;

        Ok(())
    }
}
