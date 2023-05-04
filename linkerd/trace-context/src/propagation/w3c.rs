use http::header::HeaderName;
use tracing::{debug, trace};

use super::{decode_id_with_padding, get_header_str, Propagation, TraceContext};
use crate::{Flags, Id};

static HTTP_TRACEPARENT: HeaderName = HeaderName::from_static("traceparent");
const VERSION_00: &str = "00";

pub fn unpack_w3c_trace_context<B>(request: &http::Request<B>) -> Option<TraceContext> {
    get_header_str(request, &HTTP_TRACEPARENT).and_then(parse_context)
}

/// Given an http request and a w3c trace context, create a new Span ID and
/// assign it to the tracecontext header value, in order to propagate
/// the trace context downstream.
pub fn increment_http_span_id<B>(request: &mut http::Request<B>, context: &TraceContext) -> Id {
    let span_id = Id::new_span_id(&mut rand::thread_rng());

    trace!(%span_id, "Incremented span id");

    let new_header = {
        let mut buf = String::with_capacity(60);
        buf.push_str(VERSION_00);
        buf.push('-');
        buf.push_str(&hex::encode(context.trace_id.as_ref()));
        buf.push('-');
        buf.push_str(&hex::encode(span_id.as_ref()));
        buf.push('-');
        buf.push_str(&hex::encode(vec![context.flags.0]));
        buf
    };

    if let Ok(hv) = http::HeaderValue::from_str(&new_header) {
        request.headers_mut().insert(&HTTP_TRACEPARENT, hv);
    } else {
        debug!(header = %HTTP_TRACEPARENT, header_value = %new_header, "Invalid non-ASCII or control character in header value");
    }

    span_id
}

/// Parse a given header value as a w3c TraceContext value.
fn parse_context(header_value: &str) -> Option<TraceContext> {
    let rest = match header_value.split_once('-') {
        Some((version, rest)) => {
            if version != VERSION_00 {
                debug!(header = %HTTP_TRACEPARENT, %header_value, %version, "Tracecontext header value contains invalid version",);
                return None;
            }
            rest
        }
        None => {
            debug!(header = %HTTP_TRACEPARENT, %header_value, "Tracecontext header value does not contain version");
            return None;
        }
    };

    let (trace_id, rest) = parse_header_value(rest, 16)?;
    let (parent_id, rest) = parse_header_value(rest, 8)?;

    let flags = match hex::decode(rest) {
        // If valid hex, take final bit and AND with 1. W3C only uses one bit
        // for flags in version 00, and the bit is used to control sampling
        Ok(decoded) => Flags(decoded[0]),
        // If invalid hex, invalidate trace
        Err(error) => {
            debug!(header = %HTTP_TRACEPARENT, flags = %rest, %error, "Failed to hex decode tracecontext flags");
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
fn parse_header_value(next_header_value: &str, pad_to: usize) -> Option<(Id, &str)> {
    let next_parse_result = next_header_value
        .split_once('-')
        .filter(|(id, _)| !id.chars().all(|c| c == '0'));
    if next_parse_result.is_none() {
        debug!(header = %HTTP_TRACEPARENT, "Id in header value contains invalid all zeros value");
    }
    next_parse_result
        .and_then(|(id, rest)| decode_id_with_padding(id, pad_to)
                  .map_err(|error| debug!(header = %HTTP_TRACEPARENT, %error, %id, "Id in header value contains invalid hex"))
                  .ok()
                  .zip(Some(rest)))
}

#[cfg(test)]
mod tests {
    use super::parse_context;

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
}
