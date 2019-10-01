use crate::trace_context;
use linkerd2_error::Error;
use opencensus_proto::trace::v1 as oc;
use std::collections::HashMap;
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

    fn mk_span(&self, mut span: trace_context::Span) -> oc::Span {
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
        oc::Span {
            trace_id: span.trace_id.into(),
            span_id: span.span_id.into(),
            tracestate: None,
            parent_span_id: span.parent_id.into(),
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
        }
    }
}

impl trace_context::SpanSink for SpanConverter {
    fn try_send(&mut self, span: trace_context::Span) -> Result<(), Error> {
        let span = self.mk_span(span);
        self.sink.try_send(span).map_err(Into::into)
    }
}

fn truncatable(value: String) -> oc::TruncatableString {
    oc::TruncatableString {
        value,
        truncated_byte_count: 0,
    }
}
