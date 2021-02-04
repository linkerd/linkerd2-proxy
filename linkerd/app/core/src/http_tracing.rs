use linkerd_error::Error;
use linkerd_opencensus::proto::trace::v1 as oc;
use linkerd_stack::layer;
use linkerd_trace_context::{self as trace_context, TraceContext};
use std::{collections::HashMap, error, fmt, sync::Arc};
use tokio::sync::mpsc;

pub type SpanSink = Option<mpsc::Sender<oc::Span>>;
pub type Labels = Arc<HashMap<String, String>>;

/// SpanConverter converts trace_context::Span objects into OpenCensus agent
/// protobuf span objects. SpanConverter receives trace_context::Span objects by
/// implmenting the SpanSink trait. For each span that it receives, it converts
/// it to an OpenCensus span and then sends it on the provided mpsc::Sender.
#[derive(Clone)]
pub struct SpanConverter {
    kind: Kind,
    sink: mpsc::Sender<oc::Span>,
    labels: Labels,
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

pub fn server<S>(
    sink: SpanSink,
    labels: impl Into<Labels>,
) -> impl layer::Layer<S, Service = TraceContext<Option<SpanConverter>, S>> + Clone {
    SpanConverter::layer(Kind::Server, sink, labels)
}

pub fn client<S>(
    sink: SpanSink,
    labels: impl Into<Labels>,
) -> impl layer::Layer<S, Service = TraceContext<Option<SpanConverter>, S>> + Clone {
    SpanConverter::layer(Kind::Client, sink, labels)
}

#[derive(Copy, Clone, Debug, PartialEq)]
enum Kind {
    Server = 1,
    Client = 2,
}

impl SpanConverter {
    fn layer<S>(
        kind: Kind,
        sink: SpanSink,
        labels: impl Into<Labels>,
    ) -> impl layer::Layer<S, Service = TraceContext<Option<Self>, S>> + Clone {
        TraceContext::layer(sink.map(move |sink| Self {
            kind,
            sink,
            labels: labels.into(),
        }))
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
                k.to_string(),
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
            kind: self.kind as i32,
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
            same_process_as_parent_span: Some(self.kind == Kind::Client),
            child_span_count: None,
        })
    }
}

impl trace_context::SpanSink for SpanConverter {
    #[inline]
    fn is_enabled(&self) -> bool {
        true
    }

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
