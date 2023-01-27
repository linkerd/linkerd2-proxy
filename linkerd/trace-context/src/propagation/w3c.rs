use http::HeaderValue;
use tracing::{trace, warn};

use super::{decode_id_with_padding, get_header_str, Propagation, TraceContext};
use crate::{Flags, Id};

pub const HTTP_TRACEPARENT: &str = "traceparent";
pub const HEADER_VERSION_00: &str = "00";

pub fn unpack_w3c_trace_context<B>(request: &http::Request<B>) -> Option<TraceContext> {
    get_header_str(request, HTTP_TRACEPARENT).and_then(parse_context)
}

/// Given an http request and a w3c trace context, create a new Span ID and
/// assign it to the tracecontext header value, in order to propagate
/// the trace context downstream.
pub fn increment_http_span_id<B>(request: &mut http::Request<B>, context: &TraceContext) -> Id {
    let span_id = Id::new_span_id(&mut rand::thread_rng());

    trace!("incremented span id: {}", span_id);

    let new_header = {
        let mut buf = String::with_capacity(60);
        buf.push_str(HEADER_VERSION_00);
        buf.push('-');
        buf.push_str(&hex::encode(context.trace_id.as_ref()));
        buf.push('-');
        buf.push_str(&hex::encode(context.parent_id.as_ref()));
        buf.push('-');
        buf.push_str(&hex::encode(vec![context.flags.0]));
        buf
    };

    if let Result::Ok(hv) = HeaderValue::from_str(&new_header) {
        request.headers_mut().insert(HTTP_TRACEPARENT, hv);
    } else {
        warn!("invalid value {} for tracecontext header", new_header);
    }

    span_id
}

/// Parse a given header value as a w3c TraceContext value.
fn parse_context(header_value: &str) -> Option<TraceContext> {
    let rest = match header_value.split_once('-') {
        Some((version, rest)) => {
            if version != HEADER_VERSION_00 {
                warn!("Tracecontext header {header_value} contains invalid version: {version}",);
                return None;
            }
            rest
        }
        None => {
            warn!("Tracecontext header {header_value} is invalid");
            return None;
        }
    };

    let (trace_id, rest) = if let Some((id, rest)) = parse_header_value(rest, 16) {
        (id, rest)
    } else {
        warn!("Tracecontext header {header_value} contains invalid id");
        return None;
    };

    let (parent_id, rest) = if let Some((id, rest)) = parse_header_value(rest, 8) {
        (id, rest)
    } else {
        warn!("Tracecontext header {header_value} contains invalid id");
        return None;
    };

    let flags = match hex::decode(rest) {
        // If valid hex, take final bit and AND with 1. W3C only uses one bit
        // for flags in version 00, and the bit is used to control sampling
        Ok(decoded) => Flags(decoded[0]),
        // If invalid hex, invalidate trace
        Err(e) => {
            warn!("Failed to decode flags for header {header_value}: {e}");
            return None;
        }
    };

    Some(TraceContext {
        propagation: Propagation::W3CHttp,
        trace_id,
        parent_id,
        flags,
    })
}

// Parse header value as Id and return the rest after '-' separator. When an id
// is all 0 value it is considered invalid according to the spec.
// <https://www.w3.org/TR/trace-context-1/#trace-id>
fn parse_header_value(next_header: &str, pad_to: usize) -> Option<(Id, &str)> {
    next_header
        .split_once('-')
        .filter(|(id, _rest)| !id.chars().all(|c| c == '0'))
        .and_then(|(id, rest)| decode_id_with_padding(id, pad_to).zip(Some(rest)))
}

#[test]
fn w3c_context_parsed_successfully() {
    let input = "00-94d7f6ec6b95f3e916179cb6cfd01390-55ccfce77f972614-01";
    let actual = parse_context(input);

    let expected_trace = hex::decode("94d7f6ec6b95f3e916179cb6cfd01390")
        .expect("Failed to decode trace parent from hex");
    let expected_parent =
        hex::decode("55ccfce77f972614").expect("Failed to decode span parent from hex");
    let expected_flags = 1;

    assert!(actual.is_some());
    let actual = actual.unwrap();
    assert_eq!(expected_trace, actual.trace_id.0);
    assert_eq!(expected_parent, actual.parent_id.0);
    assert_eq!(expected_flags, actual.flags.0);
}

#[test]
fn w3c_context_invalid_flags() {
    let input = "00-94d7f6ec6b95f3e916179cb6cfd01390-55ccfce77f972614-011";
    let actual = parse_context(input);
    assert!(actual.is_none());

    let input = "00-94d7f6ec6b95f3e916179cb6cfd01390-55ccfce77f972614";
    let actual = parse_context(input);
    assert!(actual.is_none());
}

#[test]
fn w3c_context_invalid_version() {
    let input = "22-94d7f6ec6b95f3e916179cb6cfd01390-55ccfce77f972614-01";
    assert!(parse_context(input).is_none());

    let input = "94d7f6ec6b95f3e916179cb6cfd01390-55ccfce77f972614-01";
    assert!(parse_context(input).is_none());
}

#[test]
fn w3c_context_invalid_hex() {
    // length of id 94d(...) is odd, results in invalid hex.
    let input = "00-94d7f6ec6b95f3e916179cb6cfd013901-55ccfce77972614-01";
    assert!(parse_context(input).is_none());
}
