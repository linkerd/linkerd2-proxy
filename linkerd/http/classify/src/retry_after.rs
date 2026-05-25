//! Shared parsing for HTTP Retry-After headers and gRPC retry-pushback-ms trailers.
//!
//! This module provides pure parsing functions with no classification or store logic.
//! Both the load-biaser and the circuit breaker can use these functions to extract
//! backoff hints from HTTP and gRPC responses.

use http::{HeaderMap, StatusCode};
use std::time::Duration;

/// Parse the Retry-After header from a 429 or 503 response.
///
/// Supports two formats per RFC 7231:
/// - delay-seconds: `Retry-After: 120` -> 120 seconds
/// - HTTP-date: `Retry-After: Wed, 21 Oct 2025 07:28:00 GMT` -> duration from now
///
/// Returns `None` for:
/// - Non-429/503 responses
/// - Missing Retry-After header
/// - Invalid header formats
///
/// The returned duration is capped at `max` to prevent abuse.
pub fn parse_retry_after(
    status: StatusCode,
    headers: &HeaderMap,
    max: Duration,
) -> Option<Duration> {
    // Only parse for 429 and 503 responses
    if status != StatusCode::TOO_MANY_REQUESTS && status != StatusCode::SERVICE_UNAVAILABLE {
        return None;
    }

    let value = headers.get(http::header::RETRY_AFTER)?;
    let s = value.to_str().ok()?;

    parse_retry_after_value(s, max)
}

/// Parse a Retry-After header value string.
///
/// Tries delay-seconds first (most common), then HTTP-date format.
/// The returned duration is capped at `max`.
fn parse_retry_after_value(s: &str, max: Duration) -> Option<Duration> {
    let s = s.trim();
    // Try delay-seconds first (most common format)
    if let Ok(secs) = s.parse::<u64>() {
        let duration = Duration::from_secs(secs);
        tracing::debug!(?duration, "Parsed Retry-After delay-seconds");
        return Some(duration.min(max));
    }

    // Try HTTP-date format
    if let Ok(datetime) = httpdate::parse_http_date(s) {
        let now = std::time::SystemTime::now();
        match datetime.duration_since(now) {
            Ok(duration) => {
                tracing::debug!(?duration, "Parsed Retry-After HTTP-date");
                return Some(duration.min(max));
            }
            Err(_) => {
                tracing::debug!("Retry-After HTTP-date is in the past");
                return Some(Duration::ZERO);
            }
        }
    }

    tracing::debug!(%s, "Failed to parse Retry-After header");
    None
}

/// The grpc-retry-pushback-ms header/trailer name.
const GRPC_RETRY_PUSHBACK_MS: &str = "grpc-retry-pushback-ms";

/// Parse grpc-retry-pushback-ms from headers or trailers.
///
/// Per gRPC A6 spec:
/// - Positive i64: retry after this many milliseconds
/// - Negative i64: do not retry (returns `None`)
///
/// This function does **not** check grpc-status; that is a classification
/// concern left to the caller.
///
/// Returns `None` for:
/// - Missing header/trailer
/// - Negative values (interpreted as "do not retry")
/// - Invalid formats
///
/// The returned duration is capped at `max` to prevent abuse.
pub fn parse_grpc_retry_pushback(headers: &HeaderMap, max: Duration) -> Option<Duration> {
    let value = headers.get(GRPC_RETRY_PUSHBACK_MS)?;
    let s = value.to_str().ok()?;

    // Parse as i64 to handle potential negative values
    let ms: i64 = match s.trim().parse() {
        Ok(v) => v,
        Err(_) => {
            tracing::debug!(%s, "Failed to parse grpc-retry-pushback-ms");
            return None;
        }
    };

    // Negative values mean "do not retry".
    if ms < 0 {
        tracing::debug!(ms, "Ignoring negative grpc-retry-pushback-ms");
        return None;
    }

    let duration = Duration::from_millis(ms as u64);
    tracing::debug!(?duration, "Parsed grpc-retry-pushback-ms");
    Some(duration.min(max))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::RETRY_AFTER;
    use http::HeaderValue;
    use std::time::SystemTime;

    const MAX: Duration = Duration::from_secs(300);

    // === Retry-After tests ===

    #[test]
    fn parse_delay_seconds() {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::RETRY_AFTER, HeaderValue::from_static("120"));

        let result = parse_retry_after(StatusCode::TOO_MANY_REQUESTS, &headers, MAX);
        assert_eq!(result, Some(Duration::from_secs(120)));
    }

    #[test]
    fn parse_delay_seconds_zero() {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::RETRY_AFTER, HeaderValue::from_static("0"));

        let result = parse_retry_after(StatusCode::TOO_MANY_REQUESTS, &headers, MAX);
        assert_eq!(result, Some(Duration::ZERO));
    }

    #[test]
    fn caps_at_max() {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::RETRY_AFTER, HeaderValue::from_static("3600"));

        let result = parse_retry_after(StatusCode::TOO_MANY_REQUESTS, &headers, MAX);
        assert_eq!(result, Some(MAX));
    }

    #[test]
    fn parses_503() {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::RETRY_AFTER, HeaderValue::from_static("120"));

        let result = parse_retry_after(StatusCode::SERVICE_UNAVAILABLE, &headers, MAX);
        assert_eq!(result, Some(Duration::from_secs(120)));
    }

    #[test]
    fn ignores_other_status() {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::RETRY_AFTER, HeaderValue::from_static("120"));

        let result = parse_retry_after(StatusCode::OK, &headers, MAX);
        assert_eq!(result, None);
    }

    #[test]
    fn ignores_missing_header() {
        let headers = HeaderMap::new();

        let result = parse_retry_after(StatusCode::TOO_MANY_REQUESTS, &headers, MAX);
        assert_eq!(result, None);
    }

    #[test]
    fn ignores_invalid_value() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::RETRY_AFTER,
            HeaderValue::from_static("not-a-number"),
        );

        let result = parse_retry_after(StatusCode::TOO_MANY_REQUESTS, &headers, MAX);
        assert_eq!(result, None);
    }

    #[test]
    fn retry_after_http_date_in_past() {
        let mut headers = HeaderMap::new();
        // Use a date in the past
        headers.insert(
            RETRY_AFTER,
            "Wed, 01 Jan 2020 00:00:00 GMT".parse().unwrap(),
        );

        let result = parse_retry_after(StatusCode::TOO_MANY_REQUESTS, &headers, MAX);
        assert_eq!(result, Some(Duration::ZERO));
    }

    #[test]
    fn retry_after_http_date_in_future() {
        let target = SystemTime::now() + Duration::from_secs(60);
        let date_str = httpdate::fmt_http_date(target);
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, date_str.parse().unwrap());

        let result = parse_retry_after(StatusCode::TOO_MANY_REQUESTS, &headers, MAX);
        let dur = result.expect("should parse future HTTP-date");

        // httpdate truncates sub-seconds (1s resolution), and we can have
        // some differences for clock time under CI load or NTP adjustment
        // so test parsing correctness within a wide enough time window.
        assert!(
            dur >= Duration::from_secs(55) && dur <= Duration::from_secs(65),
            "expected ~60s, got {:?}",
            dur,
        );
    }

    #[test]
    fn retry_after_http_date_caps_at_max() {
        let target = SystemTime::now() + Duration::from_secs(600);
        let date_str = httpdate::fmt_http_date(target);
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, date_str.parse().unwrap());

        let result = parse_retry_after(StatusCode::TOO_MANY_REQUESTS, &headers, MAX);
        assert_eq!(result, Some(MAX));
    }

    // === gRPC pushback tests ===

    const MAX_GRPC: Duration = Duration::from_millis(300_000);

    #[test]
    fn parse_grpc_pushback_positive() {
        let mut headers = HeaderMap::new();
        headers.insert(GRPC_RETRY_PUSHBACK_MS, HeaderValue::from_static("5000"));

        let result = parse_grpc_retry_pushback(&headers, MAX_GRPC);
        assert_eq!(result, Some(Duration::from_millis(5000)));
    }

    #[test]
    fn parse_grpc_pushback_zero() {
        let mut headers = HeaderMap::new();
        headers.insert(GRPC_RETRY_PUSHBACK_MS, HeaderValue::from_static("0"));

        let result = parse_grpc_retry_pushback(&headers, MAX_GRPC);
        assert_eq!(result, Some(Duration::ZERO));
    }

    #[test]
    fn parse_grpc_pushback_negative() {
        let mut headers = HeaderMap::new();
        headers.insert(GRPC_RETRY_PUSHBACK_MS, HeaderValue::from_static("-1"));

        let result = parse_grpc_retry_pushback(&headers, MAX_GRPC);
        assert_eq!(result, None);
    }

    #[test]
    fn parse_grpc_pushback_caps_at_max() {
        let mut headers = HeaderMap::new();
        headers.insert(GRPC_RETRY_PUSHBACK_MS, HeaderValue::from_static("999999"));

        let result = parse_grpc_retry_pushback(&headers, MAX_GRPC);
        assert_eq!(result, Some(MAX_GRPC));
    }

    #[test]
    fn parse_grpc_pushback_missing() {
        let headers = HeaderMap::new();

        let result = parse_grpc_retry_pushback(&headers, MAX_GRPC);
        assert_eq!(result, None);
    }

    #[test]
    fn parse_grpc_pushback_invalid() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "grpc-retry-pushback-ms",
            HeaderValue::from_static("not-a-number"),
        );

        let result = parse_grpc_retry_pushback(&headers, MAX_GRPC);
        assert_eq!(result, None);
    }

    // === Whitespace handling tests ===

    #[test]
    fn retry_after_trailing_whitespace() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, "120 ".parse().unwrap());

        let result = parse_retry_after(StatusCode::TOO_MANY_REQUESTS, &headers, MAX);
        assert_eq!(result, Some(Duration::from_secs(120)));
    }

    #[test]
    fn retry_after_leading_whitespace() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, " 120".parse().unwrap());

        let result = parse_retry_after(StatusCode::TOO_MANY_REQUESTS, &headers, MAX);
        assert_eq!(result, Some(Duration::from_secs(120)));
    }

    #[test]
    fn grpc_pushback_whitespace() {
        let mut headers = HeaderMap::new();
        headers.insert(GRPC_RETRY_PUSHBACK_MS, " 5000 ".parse().unwrap());

        let result = parse_grpc_retry_pushback(&headers, MAX);
        assert_eq!(result, Some(Duration::from_millis(5000)));
    }
}
