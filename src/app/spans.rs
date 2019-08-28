use hex;
use super::dst::Direction;
use crate::proxy::http::trace;
use opencensus_proto::gen::trace::{v1 as oc};
use futures::{Async, Poll, Stream, try_ready};
use tracing::warn;
use linkerd2_proxy_core::Error;
use std::{error, fmt};

const SPAN_KIND_SERVER: i32 = 1;
const SPAN_KIND_CLIENT: i32 = 2;

pub struct SpanConverter<S> {
    direction: Direction,
    spans: S
}

#[derive(Debug)]
pub struct IdLengthError {
    id: String,
    expected_size: usize,
    actual_size: usize
}

impl error::Error for IdLengthError {
    fn description(&self) -> &str {
        "trace or span id is wrong number of bytes"
    }
}

impl fmt::Display for IdLengthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Id '{}' should have {} bytes but it has {}", self.id, self.expected_size, self.actual_size)
    }
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

    fn mk_span(&self, span: trace::Span) -> Result<oc::Span, Error> {
        Ok(oc::Span {
            trace_id: into_bytes(span.trace_id, 16)?,
            span_id: into_bytes(span.span_id, 8)?,
            tracestate: None,
            parent_span_id: into_bytes(span.parent_id, 8)?,
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
        })
    }
}

impl<S> Stream for SpanConverter<S>
where
    S: Stream<Item = trace::Span>,
{
    type Item = oc::Span;
    type Error = S::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            match try_ready!(self.spans.poll()) {
                Some(span) => match self.mk_span(span) {
                    Ok(s) => return Ok(Async::Ready(Some(s))),
                    Err(e) => warn!("Invalid span: {}", e),
                },
                None => return Ok(Async::Ready(None)),
            };
        }
    }
}

fn into_bytes(id: String, size: usize) -> Result<Vec<u8>, Error> {
    let bytes = hex::decode(&id)?;
    if bytes.len() == size {
        Ok(bytes)
    } else {
        Err(IdLengthError{
            id,
            expected_size: size,
            actual_size: bytes.len(),
        }.into())
    }
}

fn truncatable(value: String) -> oc::TruncatableString {
    oc::TruncatableString {
        value,
        truncated_byte_count: 0,
    }
}
