use crate::{Flags, Id, InsufficientBytes};
use bytes::Bytes;

use thiserror::Error;
use tracing::warn;

mod b3;
mod w3c;

#[derive(Debug)]
pub enum Propagation {
    B3Http,
    B3Grpc,
    W3CHttp,
}

#[derive(Debug)]
pub struct TraceContext {
    pub propagation: Propagation,
    pub trace_id: Id,
    pub parent_id: Id,
    pub flags: Flags,
}

#[derive(Debug, Error)]
#[error("unknown field ID {0}")]
struct UnknownFieldId(u8);

// === impl TraceContext ===

impl TraceContext {
    pub fn is_sampled(&self) -> bool {
        self.flags.is_sampled()
    }
}

/// Given an http request, attempt to unpack a distributed tracing context from
/// the headers. Only w3c and b3 context propagation formats are supported. The
/// former is tried first, and if no headers are present, function will attempt
/// to unpack as b3.
pub fn unpack_trace_context<B>(request: &http::Request<B>) -> Option<TraceContext> {
    // Attempt to parse as w3c first since it's the newest interface in
    // distributed tracing ecosystem
    w3c::unpack_w3c_trace_context(request)
        .or_else(|| b3::unpack_grpc_trace_context(request))
        .or_else(|| b3::unpack_http_trace_context(request))
}

// Generates a new span id, writes it to the request in the appropriate
// propagation format and returns the generated span id.
pub fn increment_span_id<B>(request: &mut http::Request<B>, context: &TraceContext) -> Id {
    match context.propagation {
        Propagation::B3Grpc => b3::increment_grpc_span_id(request, context),
        Propagation::B3Http => b3::increment_http_span_id(request),
        Propagation::W3CHttp => w3c::increment_http_span_id(request, context),
    }
}

// === Header parse utils ===

fn get_header_str<'a, B>(request: &'a http::Request<B>, header: &str) -> Option<&'a str> {
    let hv = request.headers().get(header)?;
    hv.to_str()
        .map_err(|e| warn!("Invalid trace header {}: {}", header, e))
        .ok()
}

// Attempt to decode a hex value to an id, padding the buffer up to the
// specified argument. Used to decode header values from hex to binary.
fn decode_id_with_padding(value: &str, pad_to: usize) -> Option<Id> {
    hex::decode(value)
        .map(|mut data| {
            if data.len() < pad_to {
                let padding = pad_to - data.len();
                let mut padded = vec![0u8; padding];
                padded.append(&mut data);
                Id(padded)
            } else {
                Id(data)
            }
        })
        .map_err(|e| warn!("Header value {} does not contain a hex: {}", value, e))
        .ok()
}

/// Attempt to split_to the given index.  If there are not enough bytes then
/// Err is returned and the given Bytes is not modified.
fn try_split_to(buf: &mut Bytes, n: usize) -> Result<Bytes, InsufficientBytes> {
    if buf.len() >= n {
        Ok(buf.split_to(n))
    } else {
        Err(InsufficientBytes)
    }
}
