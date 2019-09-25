use linkerd2_metrics::{metrics, Counter, FmtMetrics};
use std::fmt;
use std::sync::{Arc, Mutex};
use tracing::error;

metrics! {
    opencensus_span_export_streams: Counter { "Total count of opened span export streams" },
    opencensus_span_export_requests: Counter { "Total count of span export request messages" },
    opencensus_span_exports: Counter { "Total count of spans exported" }
}

struct Metrics {
    streams: Counter,
    requests: Counter,
    spans: Counter,
}

#[derive(Clone)]
pub struct Registry(Arc<Mutex<Metrics>>);

#[derive(Clone)]
pub struct Report(Arc<Mutex<Metrics>>);

pub fn new() -> (Registry, Report) {
    let metrics = Metrics {
        streams: Counter::default(),
        requests: Counter::default(),
        spans: Counter::default(),
    };
    let shared = Arc::new(Mutex::new(metrics));
    (Registry(shared.clone()), Report(shared))
}

impl Registry {
    pub fn start_stream(&mut self) {
        match self.0.lock() {
            Ok(mut metrics) => metrics.streams.incr(),
            Err(e) => error!(message="failed to lock metrics", %e),
        }
    }

    pub fn send(&mut self, spans: u64) {
        match self.0.lock() {
            Ok(mut metrics) => {
                metrics.requests.incr();
                metrics.spans += spans;
            }
            Err(e) => error!(message="failed to lock metrics", %e),
        }
    }
}

impl FmtMetrics for Report {
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let metrics = match self.0.lock() {
            Err(_) => return Ok(()),
            Ok(lock) => lock,
        };

        opencensus_span_export_streams.fmt_help(f)?;
        opencensus_span_export_streams.fmt_metric(f, metrics.streams)?;

        opencensus_span_export_requests.fmt_help(f)?;
        opencensus_span_export_requests.fmt_metric(f, metrics.requests)?;

        opencensus_span_exports.fmt_help(f)?;
        opencensus_span_exports.fmt_metric(f, metrics.spans)?;

        Ok(())
    }
}
