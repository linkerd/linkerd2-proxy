use crate::Span;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum SpanKind {
    Server = 1,
    Client = 2,
}

pub type SpanLabels = Arc<HashMap<String, String>>;

#[derive(Debug)]
pub struct ExportSpan {
    pub span: Span,
    pub kind: SpanKind,
    pub labels: SpanLabels,
}
