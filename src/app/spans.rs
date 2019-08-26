use hex;
use super::dst::Direction;
use crate::proxy::http::trace;
use opencensus_proto::gen::trace::{v1 as oc};
use futures::{Async, Poll, Stream, try_ready};
use tracing::warn;

const SPAN_KIND_SERVER: i32 = 1;
const SPAN_KIND_CLIENT: i32 = 2;

pub struct SpanConverter<S> {
    direction: Direction,
    spans: S
}

impl<S> SpanConverter<S> {
    pub fn inbound(spans: S) -> Self {
        Self {
            direction: Direction::In,
            spans,
        }
    }

    pub fn outbound(spans: S) -> Self {
        Self {
            direction: Direction::Out,
            spans,
        }
    }
}

impl<S> Stream for SpanConverter<S>
where
    S: Stream<Item = trace::Span>,
{
    type Item = oc::Span;
    type Error = S::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        let s = try_ready!(self.spans.poll()).map(|span|
            oc::Span {
                trace_id: into_bytes(span.trace_id),
                span_id: into_bytes(span.span_id),
                tracestate: None,
                parent_span_id: into_bytes(span.parent_id),
                name: Some(truncatable(span.span_name)),
                kind: match self.direction {
                    Direction::In => SPAN_KIND_SERVER,
                    Direction::Out => SPAN_KIND_CLIENT,
                },
                start_time: Some(span.start.into()),
                end_time: Some(span.end.into()),
                attributes: None,
                stack_trace: None,
                time_events: None,
                links: None,
                status: None, // TODO: record this
                resource: None,
                same_process_as_parent_span: Some(false),
                child_span_count: None,
            }
        );
        Ok(Async::Ready(s))
    }
}

fn into_bytes(id: String) -> Vec<u8> {
    hex::decode(id).unwrap_or_else(|e| {
        warn!("invalid trace id: {}", e);
        Vec::new()
    })
}

fn truncatable(value: String) -> oc::TruncatableString {
    oc::TruncatableString {
        value,
        truncated_byte_count: 0,
    }
}
