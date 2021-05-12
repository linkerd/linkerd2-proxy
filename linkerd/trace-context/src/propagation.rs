use crate::{Flags, Id, InsufficientBytes};
use bytes::Bytes;
use http::header::HeaderValue;
use linkerd_error::Error;
use rand::thread_rng;
use std::convert::TryInto;
use thiserror::Error;
use tracing::{trace, warn};

const HTTP_TRACE_ID_HEADER: &str = "x-b3-traceid";
const HTTP_SPAN_ID_HEADER: &str = "x-b3-spanid";
const HTTP_SAMPLED_HEADER: &str = "x-b3-sampled";

const GRPC_TRACE_HEADER: &str = "grpc-trace-bin";
const GRPC_TRACE_FIELD_TRACE_ID: u8 = 0;
const GRPC_TRACE_FIELD_SPAN_ID: u8 = 1;
const GRPC_TRACE_FIELD_TRACE_OPTIONS: u8 = 2;

#[derive(Debug)]
pub enum Propagation {
    Http,
    Grpc,
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

pub fn unpack_trace_context<B>(request: &http::Request<B>) -> Option<TraceContext> {
    unpack_grpc_trace_context(request).or_else(|| unpack_http_trace_context(request))
}

// Generates a new span id, writes it to the request in the appropriate
// propagation format and returns the generated span id.
pub fn increment_span_id<B>(request: &mut http::Request<B>, context: &TraceContext) -> Id {
    match context.propagation {
        Propagation::Grpc => increment_grpc_span_id(request, context),
        Propagation::Http => increment_http_span_id(request),
    }
}

fn unpack_grpc_trace_context<B>(request: &http::Request<B>) -> Option<TraceContext> {
    get_header_str(request, GRPC_TRACE_HEADER)
        .and_then(|header_str| {
            base64::decode(header_str)
                .map_err(|e| {
                    warn!(
                        "trace header {} is not base64 encoded: {}",
                        GRPC_TRACE_HEADER, e
                    )
                })
                .ok()
        })
        .and_then(|vec| {
            let mut bytes = vec.into();
            parse_grpc_trace_context_fields(&mut bytes)
        })
}

fn parse_grpc_trace_context_fields(buf: &mut Bytes) -> Option<TraceContext> {
    trace!(message = "reading binary trace context", ?buf);

    let _version = try_split_to(buf, 1).ok()?;

    let mut context = TraceContext {
        propagation: Propagation::Grpc,
        trace_id: Default::default(),
        parent_id: Default::default(),
        flags: Default::default(),
    };

    while !buf.is_empty() {
        match parse_grpc_trace_context_field(buf, &mut context) {
            Ok(()) => {}
            Err(ref e) if e.is::<UnknownFieldId>() => break,
            Err(e) => {
                warn!("error parsing {} header: {}", GRPC_TRACE_HEADER, e);
                return None;
            }
        };
    }
    Some(context)
}

fn parse_grpc_trace_context_field(
    buf: &mut Bytes,
    context: &mut TraceContext,
) -> Result<(), Error> {
    let field_id = try_split_to(buf, 1)?[0];
    match field_id {
        GRPC_TRACE_FIELD_SPAN_ID => {
            let id = try_split_to(buf, 8)?;
            trace!(
                "reading binary trace field {:?}: {:?}",
                GRPC_TRACE_FIELD_SPAN_ID,
                id
            );
            context.parent_id = id.into();
        }
        GRPC_TRACE_FIELD_TRACE_ID => {
            let id = try_split_to(buf, 16)?;
            trace!(
                "reading binary trace field {:?}: {:?}",
                GRPC_TRACE_FIELD_TRACE_ID,
                id
            );
            context.trace_id = id.into();
        }
        GRPC_TRACE_FIELD_TRACE_OPTIONS => {
            let flags = try_split_to(buf, 1)?;
            trace!(
                "reading binary trace field {:?}: {:?}",
                GRPC_TRACE_FIELD_TRACE_OPTIONS,
                flags
            );
            context.flags = flags.try_into()?;
        }
        id => {
            return Err(UnknownFieldId(id).into());
        }
    };
    Ok(())
}

// This code looks significantly weirder if some of the elements are added using
// the `vec![]` macro, despite clippy's suggestions otherwise...
#[allow(clippy::vec_init_then_push)]
fn increment_grpc_span_id<B>(request: &mut http::Request<B>, context: &TraceContext) -> Id {
    let span_id = Id::new_span_id(&mut thread_rng());

    trace!(message = "incremented span id", %span_id);

    let mut bytes = Vec::<u8>::new();

    // version
    bytes.push(0);

    // trace id
    bytes.push(GRPC_TRACE_FIELD_TRACE_ID);
    bytes.extend(context.trace_id.0.iter());

    // span id
    bytes.push(GRPC_TRACE_FIELD_SPAN_ID);
    bytes.extend(span_id.0.iter());

    // trace options
    bytes.push(GRPC_TRACE_FIELD_TRACE_OPTIONS);
    bytes.push(context.flags.0);

    let bytes_b64 = base64::encode(&bytes);

    if let Result::Ok(hv) = HeaderValue::from_str(&bytes_b64) {
        request.headers_mut().insert(GRPC_TRACE_HEADER, hv);
    } else {
        warn!("invalid header: {:?}", &bytes_b64);
    }
    span_id
}

fn unpack_http_trace_context<B>(request: &http::Request<B>) -> Option<TraceContext> {
    let parent_id = parse_header_id(request, HTTP_SPAN_ID_HEADER, 8)?;
    let trace_id = parse_header_id(request, HTTP_TRACE_ID_HEADER, 16)?;
    let flags = match get_header_str(request, HTTP_SAMPLED_HEADER) {
        Some("1") => Flags(1),
        _ => Flags(0),
    };
    Some(TraceContext {
        propagation: Propagation::Http,
        trace_id,
        parent_id,
        flags,
    })
}

fn increment_http_span_id<B>(request: &mut http::Request<B>) -> Id {
    let span_id = Id::new_span_id(&mut thread_rng());

    trace!("incremented span id: {}", span_id);

    let span_str = hex::encode(span_id.as_ref());

    if let Result::Ok(hv) = HeaderValue::from_str(&span_str) {
        request.headers_mut().insert(HTTP_SPAN_ID_HEADER, hv);
    } else {
        warn!("invalid {} header: {:?}", HTTP_SPAN_ID_HEADER, span_str);
    }
    span_id
}

fn get_header_str<'a, B>(request: &'a http::Request<B>, header: &str) -> Option<&'a str> {
    let hv = request.headers().get(header)?;
    hv.to_str()
        .map_err(|e| warn!("Invalid trace header {}: {}", header, e))
        .ok()
}

fn parse_header_id<B>(request: &http::Request<B>, header: &str, pad_to: usize) -> Option<Id> {
    let header_value = get_header_str(request, header)?;
    hex::decode(header_value)
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
        .map_err(|e| warn!("Header {} does not contain a hex value: {}", header, e))
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
