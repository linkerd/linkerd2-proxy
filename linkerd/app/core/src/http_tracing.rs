use linkerd_error::Error;
use linkerd_proxy_tap::TapTraces;
use linkerd_stack::layer;
use linkerd_trace_context::{
    self as trace_context,
    export::{ExportSpan, SpanKind, SpanLabels},
    NewTraceContext, Span,
};
use std::sync::Arc;
use tokio::sync::mpsc;

pub type SpanSink = mpsc::Sender<ExportSpan>;

pub fn server<S>(
    sink: Option<SpanSink>,
    labels: impl Into<SpanLabels>,
    tap: TapTraces,
) -> impl layer::Layer<S, Service = NewTraceContext<Option<SpanConverter>, S>> + Clone {
    NewTraceContext::layer(
        sink.map(move |sink| SpanConverter {
            kind: SpanKind::Server,
            sink,
            labels: labels.into(),
        }),
        tap,
    )
}

pub fn client<S>(
    sink: Option<SpanSink>,
    labels: impl Into<SpanLabels>,
    tap: TapTraces,
) -> impl layer::Layer<S, Service = NewTraceContext<Option<SpanConverter>, S>> + Clone {
    NewTraceContext::layer(
        sink.map(move |sink| SpanConverter {
            kind: SpanKind::Client,
            sink,
            labels: labels.into(),
        }),
        tap,
    )
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
