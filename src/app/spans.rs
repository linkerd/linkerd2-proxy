use crate::trace_context;
use opencensus_proto::trace::v1 as oc;
use std::{error, fmt};
use tokio::sync::mpsc;
use std::collections::HashMap;
use linkerd2_error::Error;

const SPAN_KIND_SERVER: i32 = 1;
const SPAN_KIND_CLIENT: i32 = 2;

#[derive(Clone)]
pub struct SpanConverter {
    kind: i32,
    sink: mpsc::Sender<oc::Span>,
    labels: HashMap<String, String>,
}

#[derive(Debug)]
pub struct IdLengthError {
    id: Vec<u8>,
    expected_size: usize,
    actual_size: usize,
}

impl error::Error for IdLengthError {
    fn description(&self) -> &str {
        "trace or span id is wrong number of bytes"
    }
}

impl fmt::Display for IdLengthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Id '{:?}' should have {} bytes but it has {}",
            self.id, self.expected_size, self.actual_size
        )
    }
}

impl SpanConverter {
    pub fn server(sink: mpsc::Sender<oc::Span>, labels: HashMap<String, String>) -> Self {
        Self {
            kind: SPAN_KIND_SERVER,
            sink,
            labels,
        }
    }

    pub fn client(sink: mpsc::Sender<oc::Span>, labels: HashMap<String, String>) -> Self {
        Self {
            kind: SPAN_KIND_CLIENT,
            sink,
            labels,
        }
    }

    fn mk_span(&self, span: trace_context::Span) -> Result<oc::Span, IdLengthError> {
        Ok(oc::Span {
            trace_id: into_bytes(span.trace_id, 16)?,
            span_id: into_bytes(span.span_id, 8)?,
            tracestate: None,
            parent_span_id: into_bytes(span.parent_id, 8)?,
            name: Some(truncatable(span.span_name)),
            kind: self.kind,
            start_time: Some(span.start.into()),
            end_time: Some(span.end.into()),
            attributes:  None, // TODO: attributes
            stack_trace: None,
            time_events: None,
            links: None,
            status: None, // TODO: record this
            resource: None,
            same_process_as_parent_span: Some(self.kind == SPAN_KIND_CLIENT),
            child_span_count: None,
        })
    }
}

impl trace_context::SpanSink for SpanConverter {
    fn try_send(&mut self, span: trace_context::Span) -> Result<(), Error> {
        let span = self.mk_span(span)?;
        self.sink.try_send(span).map_err(Into::into)
    }
}

fn into_bytes(id: trace_context::Id, size: usize) -> Result<Vec<u8>, IdLengthError> {
    let bytes: Vec<u8> = id.into();
    if bytes.len() == size {
        Ok(bytes)
    } else {
        let actual_size = bytes.len();
        Err(IdLengthError {
            id: bytes,
            expected_size: size,
            actual_size,
        })
    }
}

fn truncatable(value: String) -> oc::TruncatableString {
    oc::TruncatableString {
        value,
        truncated_byte_count: 0,
    }
}
