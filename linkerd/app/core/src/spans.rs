use linkerd2_error::Error;
use linkerd2_opencensus::proto::trace::v1 as oc;
use linkerd2_trace_context as trace_context;
use std::collections::HashMap;
use std::{error, fmt};
use tokio::sync::mpsc;

const SPAN_KIND_SERVER: i32 = 1;
const SPAN_KIND_CLIENT: i32 = 2;

/// SpanConverter converts trace_context::Span objects into OpenCensus agent
/// protobuf span objects.  SpanConverter receives trace_context::Span objects
/// by implmenting the SpanSink trait.  For each span that it receives, it
/// converts it to an OpenCensus span and then sends it on the provided
/// mpsc::Sender.
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

impl error::Error for IdLengthError {}

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

    fn mk_span(&self, mut span: trace_context::Span) -> Result<oc::Span, IdLengthError> {
        let mut attributes = HashMap::<String, oc::AttributeValue>::new();
        for (k, v) in self.labels.iter() {
            attributes.insert(
                k.clone(),
                oc::AttributeValue {
                    value: Some(oc::attribute_value::Value::StringValue(truncatable(
                        v.clone(),
                    ))),
                },
            );
        }
        for (k, v) in span.labels.drain() {
            attributes.insert(
                k,
                oc::AttributeValue {
                    value: Some(oc::attribute_value::Value::StringValue(truncatable(v))),
                },
            );
        }
        Ok(oc::Span {
            trace_id: into_bytes(span.trace_id, 16)?,
            span_id: into_bytes(span.span_id, 8)?,
            tracestate: None,
            parent_span_id: into_bytes(span.parent_id, 8)?,
            name: Some(truncatable(span.span_name)),
            kind: self.kind,
            start_time: Some(span.start.into()),
            end_time: Some(span.end.into()),
            attributes: Some(oc::span::Attributes {
                attribute_map: attributes,
                dropped_attributes_count: 0,
            }),
            stack_trace: None,
            time_events: None,
            links: None,
            status: None, // TODO: this is gRPC status; we must read response trailers to populate this
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
