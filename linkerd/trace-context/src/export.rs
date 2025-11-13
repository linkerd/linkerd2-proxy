use crate::Span;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum SpanKind {
    Server = 1,
    Client = 2,
}

impl From<SpanKind> for opentelemetry::trace::SpanKind {
    fn from(value: SpanKind) -> Self {
        match value {
            SpanKind::Server => opentelemetry::trace::SpanKind::Server,
            SpanKind::Client => opentelemetry::trace::SpanKind::Client,
        }
    }
}

pub type SpanLabels = Arc<HashMap<String, String>>;

#[derive(Debug)]
pub struct ExportSpan {
    pub span: Span,
    pub kind: SpanKind,
    pub labels: SpanLabels,
}
