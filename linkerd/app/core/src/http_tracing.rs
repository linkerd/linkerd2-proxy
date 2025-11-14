use linkerd_opentelemetry::otel::trace::SpanKind;
use linkerd_stack::layer;
use linkerd_trace_context::{export::SpanLabels, TraceContext};

pub fn server<S>(
    labels: impl Into<SpanLabels>,
) -> impl layer::Layer<S, Service = TraceContext<S>> + Clone {
    TraceContext::layer(SpanKind::Server, labels.into())
}

pub fn client<S>(
    labels: impl Into<SpanLabels>,
) -> impl layer::Layer<S, Service = TraceContext<S>> + Clone {
    TraceContext::layer(SpanKind::Client, labels.into())
}
