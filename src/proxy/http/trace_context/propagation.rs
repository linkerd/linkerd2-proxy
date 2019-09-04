use super::{Flags, Id};
use crate::Error;
use bytes::Bytes;
use http::header::HeaderValue;
use std::fmt;
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
    pub version: Id,
    pub trace_id: Id,
    pub parent_id: Id,
    pub flags: Flags,
    pub span_id: Option<Id>,
}

#[derive(Debug)]
struct InsufficientBytes;
#[derive(Debug)]
struct UnknownFieldId(u8);

// === impl InsufficientBytes ===

impl std::error::Error for InsufficientBytes {}

impl fmt::Display for InsufficientBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Insufficient bytes when decoding binary header")
    }
}

// === impl UnknownFieldId ===

impl std::error::Error for UnknownFieldId {}

impl fmt::Display for UnknownFieldId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Unknown field id {}", self.0)
    }
}

// === impl TraceContext ===

impl TraceContext {
    pub fn is_sampled(&self) -> bool {
        self.flags.is_sampled()
    }
}

pub fn unpack_trace_context<B>(request: &http::Request<B>) -> Option<TraceContext> {
    unpack_grpc_trace_context(request).or_else(|| unpack_http_trace_context(request))
}

pub fn increment_span_id<B>(request: &mut http::Request<B>, context: &mut TraceContext) {
    match context.propagation {
        Propagation::Grpc => increment_grpc_span_id(request, context),
        Propagation::Http => increment_http_span_id(request, context),
    };
}

fn unpack_grpc_trace_context<B>(request: &http::Request<B>) -> Option<TraceContext> {
    request
        .headers()
        .get(GRPC_TRACE_HEADER)
        .and_then(|hv| {
            hv.to_str()
                .map_err(|e| warn!("Invalid trace header: {}", e))
                .ok()
        })
        .and_then(|header_str| {
            base64::decode(header_str)
                .map_err(|e| warn!("Trace header is not base64 encoded: {}", e))
                .ok()
        })
        .and_then(|vec| {
            let mut bytes = vec.into();
            parse_grpc_trace_context_fields(&mut bytes)
        })
}

fn parse_grpc_trace_context_fields(buf: &mut Bytes) -> Option<TraceContext> {
    trace!("reading binary trace context: {:?}", buf);

    let version = try_split_to(buf, 1).ok()?;

    let mut context = TraceContext {
        propagation: Propagation::Grpc,
        version: version.into(),
        trace_id: Default::default(),
        parent_id: Default::default(),
        flags: Default::default(),
        span_id: None,
    };

    while buf.len() > 0 {
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
            context.flags = flags.into();
        }
        id => {
            return Err(UnknownFieldId(id).into());
        }
    };
    Ok(())
}

fn increment_grpc_span_id<B>(request: &mut http::Request<B>, context: &mut TraceContext) {
    let span_id = Id::new(8);

    trace!("incremented span id: {}", span_id);

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
    context.span_id = Some(span_id);
}

fn unpack_http_trace_context<B>(request: &http::Request<B>) -> Option<TraceContext> {
    let parent_id = parse_header_id(request, HTTP_SPAN_ID_HEADER)?;
    let trace_id = parse_header_id(request, HTTP_TRACE_ID_HEADER)?;
    let flags = match get_header_str(request, HTTP_SAMPLED_HEADER) {
        Some("1") => Flags(1),
        _ => Flags(0),
    };
    Some(TraceContext {
        propagation: Propagation::Http,
        version: Id(vec![0]),
        trace_id,
        parent_id,
        flags,
        span_id: None,
    })
}

fn increment_http_span_id<B>(request: &mut http::Request<B>, context: &mut TraceContext) {
    let span_id = Id::new(8);

    trace!("incremented span id: {}", span_id);

    let span_str = hex::encode(span_id.as_ref());

    if let Result::Ok(hv) = HeaderValue::from_str(&span_str) {
        request.headers_mut().insert(HTTP_SPAN_ID_HEADER, hv);
    } else {
        warn!("invalid {} header: {:?}", HTTP_SPAN_ID_HEADER, span_str);
    }
    context.span_id = Some(span_id);
}

fn get_header_str<'a, B>(request: &'a http::Request<B>, header: &str) -> Option<&'a str> {
    let hv = request.headers().get(header)?;
    hv.to_str()
        .map_err(|e| warn!("Invalid trace header {}: {}", header, e))
        .ok()
}

fn parse_header_id<B>(request: &http::Request<B>, header: &str) -> Option<Id> {
    let header_value = get_header_str(request, header)?;
    hex::decode(header_value)
        .map(|data| Id(data))
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
