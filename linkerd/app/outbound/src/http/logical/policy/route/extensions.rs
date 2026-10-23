use super::retry::RetryPolicy;
use linkerd_app_core::{config::ExponentialBackoff, proxy::http, svc};
use linkerd_proxy_client_policy as policy;
use std::{
    collections::BTreeSet,
    ops::RangeInclusive,
    str::FromStr,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::time;

#[derive(Clone, Debug)]
pub struct Params {
    pub retry: Option<RetryPolicy>,
    pub timeouts: policy::http::Timeouts,
    pub allow_l5d_request_headers: bool,
}

// A request extension that marks the number of times a request has been
// attempted.
#[derive(Clone, Debug)]
pub struct Attempt(pub std::num::NonZeroU16);

#[derive(Clone, Debug)]
pub struct NewSetExtensions<N> {
    inner: N,
}

#[derive(Clone, Debug)]
pub struct SetExtensions<S> {
    inner: S,
    params: Params,
}

// === impl NewSetExtensions ===

impl<N> NewSetExtensions<N> {
    pub fn layer() -> impl svc::Layer<N, Service = Self> + Clone {
        svc::layer::mk(|inner| Self { inner })
    }
}

impl<T, N> svc::NewService<T> for NewSetExtensions<N>
where
    T: svc::Param<Params>,
    N: svc::NewService<T>,
{
    type Service = SetExtensions<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let params = target.param();
        let inner = self.inner.new_service(target);
        SetExtensions { params, inner }
    }
}

// === impl SetExtensions ===

impl<B, S> svc::Service<http::Request<B>> for SetExtensions<S>
where
    S: svc::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        let retry = self.configure_retry(req.headers_mut());

        // Ensure that we get response headers within the retry timeout. Note
        // that this may be cleared super::retry::RetryPolicy::set_extensions.
        let mut timeouts = self.configure_timeouts(req.headers_mut());
        timeouts.response_headers = retry.as_ref().and_then(|r| r.timeout);

        tracing::debug!(?retry, ?timeouts, "Initializing route extensions");
        if let Some(retry) = retry {
            let _prior = req.extensions_mut().insert(retry);
            debug_assert!(_prior.is_none(), "RetryPolicy must only be configured once");
        }

        let _prior = req.extensions_mut().insert(timeouts);
        debug_assert!(
            _prior.is_none(),
            "StreamTimeouts must only be configured once"
        );

        let _prior = req.extensions_mut().insert(Attempt(1.try_into().unwrap()));
        debug_assert!(_prior.is_none(), "Attempts must only be configured once");

        self.inner.call(req)
    }
}

impl<S> SetExtensions<S> {
    fn configure_retry(&self, req: &mut http::HeaderMap) -> Option<RetryPolicy> {
        if !self.params.allow_l5d_request_headers {
            return self.params.retry.clone();
        }

        let user_retry_http = req
            .remove("l5d-retry-http")
            .and_then(|val| val.to_str().ok().and_then(parse_http_conditions));
        let user_retry_grpc = req
            .remove("l5d-retry-grpc")
            .and_then(|val| val.to_str().ok().and_then(parse_grpc_conditions));
        let user_retry_limit = req
            .remove("l5d-retry-limit")
            .and_then(|val| val.to_str().ok().and_then(|v| v.parse::<usize>().ok()));
        let user_retry_timeout = req.remove("l5d-retry-timeout").and_then(parse_duration);

        if let Some(retry) = self.params.retry.clone() {
            return Some(RetryPolicy {
                timeout: user_retry_timeout.or(retry.timeout),
                retryable_http_statuses: user_retry_http.or(retry.retryable_http_statuses.clone()),
                retryable_grpc_statuses: user_retry_grpc.or(retry.retryable_grpc_statuses.clone()),
                max_retries: user_retry_limit.unwrap_or(retry.max_retries),
                ..retry
            });
        }

        match (
            user_retry_http,
            user_retry_grpc,
            user_retry_limit,
            user_retry_timeout,
        ) {
            (None, None, None, None) => None,
            (retryable_http_statuses, retryable_grpc_statuses, retry_limit, timeout) => {
                Some(RetryPolicy {
                    timeout,
                    retryable_http_statuses,
                    retryable_grpc_statuses,
                    max_retries: retry_limit.unwrap_or(1),
                    max_request_bytes: 64 * 1024,
                    backoff: Some(ExponentialBackoff::new_unchecked(
                        std::time::Duration::from_millis(25),
                        std::time::Duration::from_millis(250),
                        1.0,
                    )),
                })
            }
        }
    }

    fn configure_timeouts(&self, req: &mut http::HeaderMap) -> http::StreamTimeouts {
        let mut timeouts = http::StreamTimeouts {
            response_headers: None,
            response_end: self.params.timeouts.response,
            idle: self.params.timeouts.idle,
            limit: self.params.timeouts.request.map(Into::into),
        };

        if !self.params.allow_l5d_request_headers {
            return timeouts;
        }

        // Accept both a shorthand and longer, more explicit version, the
        // latter taking precedence.
        if let Some(t) = req.remove("l5d-timeout").and_then(parse_duration) {
            timeouts.limit = Some(t.into());
        }
        if let Some(t) = req.remove("l5d-request-timeout").and_then(parse_duration) {
            timeouts.limit = Some(t.into());
        }

        if let Some(t) = req.remove("l5d-response-timeout").and_then(parse_duration) {
            timeouts.response_end = Some(t);
        }

        timeouts
    }
}

/// An HTTP status code accepted in `l5d-retry-http`, in `100..=599`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RetryStatus(u16);

/// One item of an `l5d-retry-http` list.
#[derive(Clone, Debug, PartialEq, Eq)]
enum HttpRetryCondition {
    ServerError,
    GatewayError,
    Status(RetryStatus),
    Range(RetryStatus, RetryStatus),
}

/// One item of an `l5d-retry-grpc` list.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum GrpcRetryCondition {
    Cancelled,
    DeadlineExceeded,
    Internal,
    ResourceExhausted,
    Unavailable,
}

#[derive(Debug, thiserror::Error)]
#[error("invalid retry condition")]
struct InvalidCondition;

fn parse_http_conditions(s: &str) -> Option<policy::http::StatusRanges> {
    let ranges = parse_conditions::<HttpRetryCondition>("l5d-retry-http", s)
        .map(RangeInclusive::from)
        .collect::<Arc<[_]>>();
    // A value with no valid item is treated as absent so that the route's
    // configured statuses still apply, rather than being replaced by an empty
    // set that disables status-based retries.
    if ranges.is_empty() {
        return None;
    }
    Some(policy::http::StatusRanges(ranges))
}

fn parse_grpc_conditions(s: &str) -> Option<policy::grpc::Codes> {
    let codes = parse_conditions::<GrpcRetryCondition>("l5d-retry-grpc", s)
        .map(|c| tonic::Code::from(c) as u16)
        .collect::<BTreeSet<_>>();
    // As with HTTP, an all-invalid value must not override the route's codes.
    if codes.is_empty() {
        return None;
    }
    Some(policy::grpc::Codes(Arc::new(codes)))
}

/// Splits a comma-separated header list into parsed items, skipping bad ones.
///
/// Items are trimmed of OWS (SP and HTAB, RFC 9110 section 5.6.1) and empty
/// items are ignored, as that section requires of list recipients. Invalid
/// items are logged and skipped so one typo does not discard the whole list.
fn parse_conditions<'s, T: FromStr>(
    header: &'static str,
    s: &'s str,
) -> impl Iterator<Item = T> + 's {
    s.split(',').filter_map(move |item| {
        let item = trim_ows(item);
        if item.is_empty() {
            return None;
        }
        match item.parse() {
            Ok(cond) => Some(cond),
            Err(_) => {
                tracing::debug!(header, condition = item, "Ignoring invalid retry condition");
                None
            }
        }
    })
}

/// Trims RFC 9110 OWS (SP and HTAB) from both ends.
fn trim_ows(s: &str) -> &str {
    s.trim_matches(|c| c == ' ' || c == '\t')
}

// === impl RetryStatus ===

impl FromStr for RetryStatus {
    type Err = InvalidCondition;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // `StatusCode` requires exactly three ASCII digits, which rejects
        // forms such as `+429` and `0429` that `u16::from_str` accepts.
        let code = s
            .parse::<http::StatusCode>()
            .map_err(|_| InvalidCondition)?
            .as_u16();
        // `StatusCode` admits 600..=999, which are not HTTP status codes.
        if code >= 600 {
            return Err(InvalidCondition);
        }
        Ok(Self(code))
    }
}

// === impl HttpRetryCondition ===

impl FromStr for HttpRetryCondition {
    type Err = InvalidCondition;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Names are checked first so that `gateway-error` is not taken as a
        // range.
        if s.eq_ignore_ascii_case("5xx") {
            return Ok(Self::ServerError);
        }
        if s.eq_ignore_ascii_case("gateway-error") {
            return Ok(Self::GatewayError);
        }

        // A range may carry OWS around the dash, e.g. `500 - 504`.
        if let Some((start, end)) = s.split_once('-') {
            let start = trim_ows(start).parse::<RetryStatus>()?;
            let end = trim_ows(end).parse::<RetryStatus>()?;
            if start > end {
                return Err(InvalidCondition);
            }
            return Ok(Self::Range(start, end));
        }

        s.parse().map(Self::Status)
    }
}

impl From<HttpRetryCondition> for RangeInclusive<u16> {
    fn from(cond: HttpRetryCondition) -> Self {
        match cond {
            HttpRetryCondition::ServerError => 500..=599,
            HttpRetryCondition::GatewayError => 502..=504,
            HttpRetryCondition::Status(RetryStatus(c)) => c..=c,
            HttpRetryCondition::Range(RetryStatus(s), RetryStatus(e)) => s..=e,
        }
    }
}

// === impl GrpcRetryCondition ===

impl FromStr for GrpcRetryCondition {
    type Err = InvalidCondition;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Only the codes that are commonly safe to retry are accepted.
        const NAMES: [(&str, GrpcRetryCondition); 5] = [
            ("cancelled", GrpcRetryCondition::Cancelled),
            ("deadline-exceeded", GrpcRetryCondition::DeadlineExceeded),
            ("internal", GrpcRetryCondition::Internal),
            ("resource-exhausted", GrpcRetryCondition::ResourceExhausted),
            ("unavailable", GrpcRetryCondition::Unavailable),
        ];
        NAMES
            .iter()
            .find(|(name, _)| s.eq_ignore_ascii_case(name))
            .map(|(_, cond)| *cond)
            .ok_or(InvalidCondition)
    }
}

impl From<GrpcRetryCondition> for tonic::Code {
    fn from(cond: GrpcRetryCondition) -> Self {
        match cond {
            GrpcRetryCondition::Cancelled => tonic::Code::Cancelled,
            GrpcRetryCondition::DeadlineExceeded => tonic::Code::DeadlineExceeded,
            GrpcRetryCondition::Internal => tonic::Code::Internal,
            GrpcRetryCondition::ResourceExhausted => tonic::Code::ResourceExhausted,
            GrpcRetryCondition::Unavailable => tonic::Code::Unavailable,
        }
    }
}

// Copied from the policy controller so that we handle the same duration values
// as we do in the YAML config.
fn parse_duration(hv: http::HeaderValue) -> Option<time::Duration> {
    #[inline]
    fn parse(s: &str) -> Option<time::Duration> {
        let s = s.trim();
        let offset = s.rfind(|c: char| c.is_ascii_digit())?;
        let (magnitude, unit) = s.split_at(offset + 1);
        let magnitude = magnitude.parse::<u64>().ok()?;

        let mul = match unit {
            "" if magnitude == 0 => 0,
            "ms" => 1,
            "s" => 1000,
            "m" => 1000 * 60,
            "h" => 1000 * 60 * 60,
            "d" => 1000 * 60 * 60 * 24,
            _ => return None,
        };

        let ms = magnitude.checked_mul(mul)?;
        Some(time::Duration::from_millis(ms))
    }
    let s = hv.to_str().ok()?;
    let Some(d) = parse(s) else {
        tracing::debug!("Invalid duration: {:?}", s);
        return None;
    };
    Some(d)
}

#[cfg(test)]
mod tests {
    use super::{
        parse_grpc_conditions, parse_http_conditions, GrpcRetryCondition, HttpRetryCondition,
        RetryStatus,
    };
    use std::{collections::BTreeSet, ops::RangeInclusive};
    use tonic::Code;

    /// Projects parser results onto plain values so assertions do not depend on wrapper types.
    struct Parsed;

    impl Parsed {
        /// Parses an `l5d-retry-http` value into its status ranges, in header order.
        fn http(value: &str) -> Option<Vec<RangeInclusive<u16>>> {
            parse_http_conditions(value).map(|ranges| ranges.0.to_vec())
        }

        /// Parses an `l5d-retry-grpc` value into its set of numeric codes.
        fn grpc(value: &str) -> Option<BTreeSet<u16>> {
            parse_grpc_conditions(value).map(|codes| codes.0.as_ref().clone())
        }

        /// Builds the expected set of numeric codes from tonic codes.
        fn codes(codes: &[Code]) -> Option<BTreeSet<u16>> {
            Some(codes.iter().map(|c| *c as u16).collect())
        }
    }

    // === RetryStatus ===

    /// Three-digit codes in `100..=599` parse, including both bounds.
    ///
    /// Sets up `100`, `429` and `599`.
    /// Catches an off-by-one in the `600` upper bound.
    #[test]
    fn retry_status_valid() {
        // Both bounds and a typical code.
        for (value, code) in [("100", 100), ("429", 429), ("599", 599)] {
            assert_eq!(
                value.parse::<RetryStatus>().ok(),
                Some(RetryStatus(code)),
                "{value:?} must parse"
            );
        }
    }

    /// Codes outside `100..=599` and non-canonical forms are rejected.
    ///
    /// Sets up `99`, `600`, `999`, `+429`, `0429`, `42x` and an empty string.
    /// Catches `u16::from_str` leniency and `StatusCode` admitting 600..=999.
    #[test]
    fn retry_status_invalid() {
        // Each value is not an HTTP status code.
        for value in ["99", "600", "999", "+429", "0429", "42x", ""] {
            assert!(
                value.parse::<RetryStatus>().is_err(),
                "{value:?} must be rejected"
            );
        }
    }

    /// OWS is not trimmed by the status parser itself.
    ///
    /// Sets up ` 429` and `429\t`.
    /// Catches a status parser that silently trims, hiding missing trims in callers.
    #[test]
    fn retry_status_rejects_ows() {
        // Trimming is the list splitter's and range parser's job.
        for value in [" 429", "429\t"] {
            assert!(
                value.parse::<RetryStatus>().is_err(),
                "{value:?} must be rejected"
            );
        }
    }

    // === HttpRetryCondition ===

    /// Names, codes and ranges each parse to their variant.
    ///
    /// Sets up `5xx`, `gateway-error`, `429` and `500-504`.
    /// Catches `gateway-error` reaching the range branch via its dash.
    #[test]
    fn http_condition_valid() {
        // One value per variant.
        assert_eq!(
            "5xx".parse::<HttpRetryCondition>().ok(),
            Some(HttpRetryCondition::ServerError)
        );
        assert_eq!(
            "gateway-error".parse::<HttpRetryCondition>().ok(),
            Some(HttpRetryCondition::GatewayError)
        );
        assert_eq!(
            "429".parse::<HttpRetryCondition>().ok(),
            Some(HttpRetryCondition::Status(RetryStatus(429)))
        );
        assert_eq!(
            "500-504".parse::<HttpRetryCondition>().ok(),
            Some(HttpRetryCondition::Range(
                RetryStatus(500),
                RetryStatus(504)
            ))
        );
    }

    /// Names match ASCII case-insensitively.
    ///
    /// Sets up `5XX` and `Gateway-Error`.
    /// Catches an exact-match comparison, which `main` did not use.
    #[test]
    fn http_condition_case_insensitive() {
        // Upper and mixed case.
        assert_eq!(
            "5XX".parse::<HttpRetryCondition>().ok(),
            Some(HttpRetryCondition::ServerError)
        );
        assert_eq!(
            "Gateway-Error".parse::<HttpRetryCondition>().ok(),
            Some(HttpRetryCondition::GatewayError)
        );
    }

    /// OWS around the range dash is trimmed.
    ///
    /// Sets up `500 - 504` and `500-\t504`.
    /// Catches a range parser that does not trim each half.
    #[test]
    fn http_condition_range_ows() {
        // SP and HTAB variants.
        for value in ["500 - 504", "500-\t504"] {
            assert_eq!(
                value.parse::<HttpRetryCondition>().ok(),
                Some(HttpRetryCondition::Range(
                    RetryStatus(500),
                    RetryStatus(504)
                )),
                "{value:?} must parse as a range"
            );
        }
    }

    /// Unknown names, reversed or open ranges and bad codes are rejected.
    ///
    /// Sets up `teapot`, `504-500`, `500-`, `-504`, `500-600` and `4xx`.
    /// Catches accepting a half-parsed range or an unknown class name.
    #[test]
    fn http_condition_invalid() {
        // Each value is not a supported condition.
        for value in ["teapot", "504-500", "500-", "-504", "500-600", "4xx"] {
            assert!(
                value.parse::<HttpRetryCondition>().is_err(),
                "{value:?} must be rejected"
            );
        }
    }

    /// Each variant maps to the status range `main` used.
    ///
    /// Sets up one value per variant.
    /// Catches a wrong range for `5xx` or `gateway-error`.
    #[test]
    fn http_condition_into_range() {
        // Compare each variant's range.
        assert_eq!(
            RangeInclusive::from(HttpRetryCondition::ServerError),
            500..=599
        );
        assert_eq!(
            RangeInclusive::from(HttpRetryCondition::GatewayError),
            502..=504
        );
        assert_eq!(
            RangeInclusive::from(HttpRetryCondition::Status(RetryStatus(429))),
            429..=429
        );
        assert_eq!(
            RangeInclusive::from(HttpRetryCondition::Range(
                RetryStatus(500),
                RetryStatus(504)
            )),
            500..=504
        );
    }

    // === GrpcRetryCondition ===

    /// Every supported name parses and maps to its tonic code.
    ///
    /// Sets up the five names `main` accepted.
    /// Catches a dropped or mismapped name.
    #[test]
    fn grpc_condition_valid() {
        // Each name with its expected code.
        for (value, code) in [
            ("cancelled", Code::Cancelled),
            ("deadline-exceeded", Code::DeadlineExceeded),
            ("internal", Code::Internal),
            ("resource-exhausted", Code::ResourceExhausted),
            ("unavailable", Code::Unavailable),
        ] {
            let cond = value.parse::<GrpcRetryCondition>().ok();
            assert_eq!(cond.map(Code::from), Some(code), "{value:?} must parse");
        }
    }

    /// Names match ASCII case-insensitively.
    ///
    /// Sets up `UNAVAILABLE` and `Deadline-Exceeded`.
    /// Catches an exact-match comparison, which `main` did not use.
    #[test]
    fn grpc_condition_case_insensitive() {
        // Upper and mixed case.
        assert_eq!(
            "UNAVAILABLE".parse::<GrpcRetryCondition>().ok(),
            Some(GrpcRetryCondition::Unavailable)
        );
        assert_eq!(
            "Deadline-Exceeded".parse::<GrpcRetryCondition>().ok(),
            Some(GrpcRetryCondition::DeadlineExceeded)
        );
    }

    /// Unknown names, numeric codes and untrimmed names are rejected.
    ///
    /// Sets up `unavailble`, `not-found`, `14`, ` unavailable` and an empty string.
    /// Catches accepting codes outside the supported set or trimming in the wrong layer.
    #[test]
    fn grpc_condition_invalid() {
        // Each value is not a supported name.
        for value in ["unavailble", "not-found", "14", " unavailable", ""] {
            assert!(
                value.parse::<GrpcRetryCondition>().is_err(),
                "{value:?} must be rejected"
            );
        }
    }

    // === HTTP ===

    /// SP after a list comma is trimmed, so every item is kept.
    ///
    /// Sets up `5xx, 429`, the form most clients emit for list headers.
    /// Catches matching raw split items, which drops ` 429`.
    #[test]
    fn http_space_after_comma() {
        // RFC 9110 section 5.6.1 allows OWS around list commas.
        assert_eq!(
            Parsed::http("5xx, 429"),
            Some(vec![500..=599, 429..=429]),
            "space after comma must not drop the next item"
        );
    }

    /// SP before a list comma is trimmed, so every item is kept.
    ///
    /// Sets up `5xx ,429`.
    /// Catches matching raw split items, which drops `5xx `.
    #[test]
    fn http_space_before_comma() {
        // Trailing OWS on an item is as valid as leading OWS.
        assert_eq!(
            Parsed::http("5xx ,429"),
            Some(vec![500..=599, 429..=429]),
            "space before comma must not drop the previous item"
        );
    }

    /// HTAB on either side of a list comma is trimmed.
    ///
    /// Sets up `5xx,\t429` and `5xx\t,429`.
    /// Catches a fix that trims SP but not HTAB, which RFC 9110 OWS also allows.
    #[test]
    fn http_tab_around_comma() {
        // Tab after the comma.
        assert_eq!(
            Parsed::http("5xx,\t429"),
            Some(vec![500..=599, 429..=429]),
            "tab after comma must not drop the next item"
        );
        // Tab before the comma.
        assert_eq!(
            Parsed::http("5xx\t,429"),
            Some(vec![500..=599, 429..=429]),
            "tab before comma must not drop the previous item"
        );
    }

    /// A numeric range after a spaced comma is kept.
    ///
    /// Sets up `5xx, 500-504`.
    /// Catches ` 500` failing `u16` parsing when the range start is not trimmed.
    #[test]
    fn http_range_after_spaced_comma() {
        // The range start carries the leading OWS from the list separator.
        assert_eq!(
            Parsed::http("5xx, 500-504"),
            Some(vec![500..=599, 500..=504]),
            "range after a spaced comma must be kept"
        );
    }

    /// Spaces around the range dash are trimmed.
    ///
    /// Sets up `500 - 504`, `500- 504` and `500 -504`.
    /// Catches trimming only item edges, which leaves `500 ` or ` 504` unparseable.
    #[test]
    fn http_range_with_inner_spaces() {
        // Each spacing variant must yield the same range.
        for value in ["500 - 504", "500- 504", "500 -504"] {
            assert_eq!(
                Parsed::http(value),
                Some(vec![500..=504]),
                "range {value:?} must parse as 500..=504"
            );
        }
    }

    /// Empty list items are skipped.
    ///
    /// Sets up `5xx,,429` and a trailing comma `5xx,`.
    /// Catches a fix that rejects the whole value on an empty item.
    #[test]
    fn http_empty_items_skipped() {
        // RFC 9110 section 5.6.1 requires recipients to ignore empty list elements.
        assert_eq!(
            Parsed::http("5xx,,429"),
            Some(vec![500..=599, 429..=429]),
            "empty middle item must be skipped"
        );
        // A trailing comma leaves one empty element.
        assert_eq!(
            Parsed::http("5xx,"),
            Some(vec![500..=599]),
            "trailing comma must be skipped"
        );
    }

    /// Condition names match case-insensitively, with or without spaces.
    ///
    /// Sets up `5XX,Gateway-Error` and `5xx, Gateway-Error`.
    /// Catches a fix that trims but compares names case-sensitively.
    #[test]
    fn http_names_case_insensitive() {
        // No whitespace: only case differs.
        assert_eq!(
            Parsed::http("5XX,Gateway-Error"),
            Some(vec![500..=599, 502..=504]),
            "names must match case-insensitively"
        );
        // Whitespace and mixed case together.
        assert_eq!(
            Parsed::http("5xx, Gateway-Error"),
            Some(vec![500..=599, 502..=504]),
            "mixed-case name after a spaced comma must be kept"
        );
    }

    /// A status code with a leading `+` is rejected.
    ///
    /// Sets up `5xx,+429`.
    /// Catches relying on `u16::from_str`, which accepts a leading `+` that is not a status code.
    #[test]
    fn http_leading_plus_rejected() {
        // Only `5xx` is a valid condition here.
        assert_eq!(
            Parsed::http("5xx,+429"),
            Some(vec![500..=599]),
            "+429 must not parse as a status code"
        );
    }

    /// Unknown items are dropped while valid ones are kept.
    ///
    /// Sets up `5xx,teapot` and `5xx,600`.
    /// Catches a fix that rejects the whole value because of one bad item.
    #[test]
    fn http_unknown_items_dropped() {
        // An unknown name.
        assert_eq!(
            Parsed::http("5xx,teapot"),
            Some(vec![500..=599]),
            "unknown name must be dropped"
        );
        // An out-of-range code.
        assert_eq!(
            Parsed::http("5xx,600"),
            Some(vec![500..=599]),
            "out-of-range code must be dropped"
        );
    }

    /// A value with no valid item yields `None`, so the route's retry statuses apply.
    ///
    /// Sets up names, separators, codes and ranges that are all invalid.
    /// Catches `Some(empty)`, which silently replaces the route's configured statuses.
    #[test]
    fn http_all_invalid_is_none() {
        // Each value has no item that parses.
        for value in ["teapot", "5xx;429", " , ", "600", "504-500", "+429"] {
            assert_eq!(
                Parsed::http(value),
                None,
                "{value:?} must yield None so the route config applies"
            );
        }
    }

    // === gRPC ===

    /// SP or HTAB after a list comma is trimmed for gRPC codes.
    ///
    /// Sets up `unavailable, cancelled` and `unavailable,\tcancelled`.
    /// Catches matching raw split items, which drops ` cancelled`.
    #[test]
    fn grpc_whitespace_around_comma() {
        // RFC 9110 section 5.6.1 allows OWS around list commas.
        for value in [
            "unavailable, cancelled",
            "unavailable ,cancelled",
            "unavailable,\tcancelled",
        ] {
            assert_eq!(
                Parsed::grpc(value),
                Parsed::codes(&[Code::Unavailable, Code::Cancelled]),
                "{value:?} must keep both codes"
            );
        }
    }

    /// gRPC code names match case-insensitively, with or without spaces.
    ///
    /// Sets up `UNAVAILABLE,Cancelled` and `UNAVAILABLE, Cancelled`.
    /// Catches a fix that trims but compares names case-sensitively.
    #[test]
    fn grpc_names_case_insensitive() {
        // No whitespace: only case differs.
        assert_eq!(
            Parsed::grpc("UNAVAILABLE,Cancelled"),
            Parsed::codes(&[Code::Unavailable, Code::Cancelled]),
            "names must match case-insensitively"
        );
        // Whitespace and mixed case together.
        assert_eq!(
            Parsed::grpc("UNAVAILABLE, Cancelled"),
            Parsed::codes(&[Code::Unavailable, Code::Cancelled]),
            "mixed-case name after a spaced comma must be kept"
        );
    }

    /// Empty and unknown gRPC items are skipped while valid ones are kept.
    ///
    /// Sets up `unavailable,,cancelled` and `unavailable,not-found`.
    /// Catches a fix that rejects the whole value because of one bad item.
    #[test]
    fn grpc_empty_and_unknown_items_skipped() {
        // Empty list elements are ignored.
        assert_eq!(
            Parsed::grpc("unavailable,,cancelled"),
            Parsed::codes(&[Code::Unavailable, Code::Cancelled]),
            "empty item must be skipped"
        );
        // `not-found` is not a supported retry condition.
        assert_eq!(
            Parsed::grpc("unavailable,not-found"),
            Parsed::codes(&[Code::Unavailable]),
            "unknown name must be dropped"
        );
    }

    /// A gRPC value with no valid item yields `None`, so the route's retry codes apply.
    ///
    /// Sets up a misspelled code and whitespace-only items.
    /// Catches `Some(empty)`, which silently replaces the route's configured codes.
    #[test]
    fn grpc_all_invalid_is_none() {
        // Each value has no item that parses.
        for value in ["unavailble", " , "] {
            assert_eq!(
                Parsed::grpc(value),
                None,
                "{value:?} must yield None so the route config applies"
            );
        }
    }
}
