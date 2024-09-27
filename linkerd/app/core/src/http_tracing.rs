use linkerd_error::Error;
use linkerd_stack::layer;
use linkerd_trace_context::{
    self as trace_context,
    export::{ExportSpan, SpanKind, SpanLabels},
    Span, TraceContext,
};
use std::{str::FromStr, sync::Arc};
use tokio::sync::mpsc;

#[derive(Debug, Copy, Clone, Default)]
pub enum CollectorProtocol {
    #[default]
    OpenCensus,
    OpenTelemetry,
}

impl FromStr for CollectorProtocol {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("opencensus") {
            Ok(Self::OpenCensus)
        } else if s.eq_ignore_ascii_case("opentelemetry") {
            Ok(Self::OpenTelemetry)
        } else {
            Err(())
        }
    }
}

pub type SpanSink = mpsc::Sender<ExportSpan>;

pub fn server<S>(
    sink: Option<SpanSink>,
    labels: impl Into<SpanLabels>,
) -> impl layer::Layer<S, Service = TraceContext<Option<SpanConverter>, S>> + Clone {
    TraceContext::layer(sink.map(move |sink| SpanConverter {
        kind: SpanKind::Server,
        sink,
        labels: labels.into(),
    }))
}

pub fn client<S>(
    sink: Option<SpanSink>,
    labels: impl Into<SpanLabels>,
) -> impl layer::Layer<S, Service = TraceContext<Option<SpanConverter>, S>> + Clone {
    TraceContext::layer(sink.map(move |sink| SpanConverter {
        kind: SpanKind::Client,
        sink,
        labels: labels.into(),
    }))
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
