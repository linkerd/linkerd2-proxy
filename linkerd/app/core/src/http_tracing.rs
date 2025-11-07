use linkerd_error::Error;
use linkerd_stack::layer;
use linkerd_trace_context::{
    self as trace_context,
    export::{ExportSpan, SpanKind, SpanLabels},
    Span, TraceContext,
};
use std::sync::Arc;
use tokio::sync::mpsc;

pub type SpanSink = mpsc::Sender<ExportSpan>;

pub fn server<S>(
    labels: impl Into<SpanLabels>,
) -> impl layer::Layer<S, Service = TraceContext<S>> + Clone {
    TraceContext::layer(opentelemetry::trace::SpanKind::Server, labels.into())
}

pub fn client<S>(
    labels: impl Into<SpanLabels>,
) -> impl layer::Layer<S, Service = TraceContext<S>> + Clone {
    TraceContext::layer(opentelemetry::trace::SpanKind::Client, labels.into())
}

#[derive(Clone)]
pub struct SpanConverter {
    kind: SpanKind,
    sink: SpanSink,
    labels: SpanLabels,
}

impl trace_context::SpanSink for SpanConverter {
    fn is_enabled(&self) -> bool {
        true
    }

    fn try_send(&mut self, span: Span) -> Result<(), Error> {
        self.sink.try_send(ExportSpan {
            span,
            kind: self.kind,
            labels: Arc::clone(&self.labels),
        })?;
        Ok(())
    }
}
