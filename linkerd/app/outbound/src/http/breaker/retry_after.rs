//! Retry-After and grpc-retry-pushback-ms parsing for rate limiting responses.
//!
//! Backoff hints from HTTP 429/503 and gRPC RESOURCE_EXHAUSTED responses are
//! parsed into [`RetryAfterStore`] and [`GrpcRetryPushbackStore`]. Each store
//! keeps the latest hint it receives, so a recovering server that lowers its
//! hint is honored at the lower value and an earlier larger value no longer
//! pins it. A breaker queries both stores through [`take_combined_hint`] and
//! uses the larger of the two sources, clamped to its backoff bounds.
//!
//! Parsing itself is delegated to [`linkerd_http_classify::retry_after`].
//!
//! ```text
//! HTTP 429/503 Response
//!     |
//!     v
//! RetryAfterClassify::start()
//!     |
//!     +-> parse_retry_after() -> RetryAfterStore::record()
//!     |
//!     +-> gated_parse_grpc_retry_pushback() (from headers, for unary gRPC)
//!             |
//!             v
//!         GrpcRetryPushbackStore::record()
//!
//! gRPC RESOURCE_EXHAUSTED (streaming)
//!     |
//!     v
//! GrpcRetryPushbackClassifyEos::eos(trailers)
//!     |
//!     v
//! gated_parse_grpc_retry_pushback() -> GrpcRetryPushbackStore::record()
//!
//! Circuit Breaker Backoff
//!     |
//!     v
//! take_combined_hint()
//!     |
//!     +-> RetryAfterStore::take()
//!     +-> GrpcRetryPushbackStore::take()
//!             |
//!             v
//!         max(http_hint, grpc_hint), clamped to [min, max] backoff
//! ```

use http::HeaderMap;
use linkerd_app_core::classify::{self, grpc_code};
use linkerd_http_classify::{ClassifyEos, ClassifyResponse};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tonic::Code;

/// Shared store for duration hints, timestamped to track how old they are.
#[derive(Clone, Debug)]
pub struct DurationHintStore {
    name: &'static str,
    inner: Arc<Mutex<Option<(Instant, Duration)>>>,
}

impl DurationHintStore {
    /// Create a new empty store with the given name for logging.
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            inner: Arc::new(Mutex::new(None)),
        }
    }

    /// Record a duration hint, keeping the latest value.
    ///
    /// Each call overwrites any stored hint with the new value and a fresh
    /// timestamp. A recovering server that lowers its `Retry-After` is then
    /// honored at the lower value, and a server that repeats the same hint keeps
    /// it from aging out. The hint is only ever applied as a backoff floor with
    /// `max(hint, backoff)` clamped to the backoff bounds, so a smaller fresh hint
    /// can never shorten the wait.
    pub fn record(&self, duration: Duration) {
        *self.inner.lock() = Some((Instant::now(), duration));
        tracing::debug!(
            ?duration,
            hint_type = %self.name,
            "Recorded hint (last value wins)",
        );
    }

    /// Take the stored hint if it exists and was recorded recently.
    ///
    /// A recent hint is consumed and set to None so the same hint is not reused
    /// across backoff cycles, while `max_age` sets how old a hint may be before
    /// it is dropped. The timestamp is inspected first and only a recent hint is
    /// taken, so an old hint is dropped without being consumed. This avoids a race
    /// where an old hint is consumed and clears the store just as a new hint
    /// arrives.
    pub fn take(&self, max_age: Duration) -> Option<Duration> {
        let mut guard = self.inner.lock();
        if let Some(&(recorded_at, duration)) = guard.as_ref() {
            // Monotonic clock: saturating_duration_since returns zero on backwards drift
            let elapsed = Instant::now().saturating_duration_since(recorded_at);
            if elapsed <= max_age {
                // Elapsed time consumed the entire hint.
                if elapsed >= duration {
                    *guard = None;
                    tracing::debug!(
                        ?duration,
                        ?elapsed,
                        hint_type = %self.name,
                        "Hint fully elapsed, discarding",
                    );
                    return None;
                }

                // Subtract elapsed time to obtain remaining wait.
                let adjusted = duration.saturating_sub(elapsed);

                *guard = None;
                tracing::debug!(
                    ?duration,
                    ?elapsed,
                    ?adjusted,
                    hint_type = %self.name,
                    "Using hint (adjusted for elapsed time)",
                );
                return Some(adjusted);
            }
            tracing::debug!(
                ?duration,
                ?elapsed,
                hint_type = %self.name,
                "Hint too old, discarding",
            );

            // The hint is too old. Discard it explicitly without consuming for use.
            *guard = None;
        }
        None
    }
}

/// A shared store for Retry-After hints.
///
/// It passes Retry-After hints from the response path to the circuit breaker and
/// wraps [`DurationHintStore`] with the "Retry-After" name for logging.
#[derive(Clone, Debug)]
pub struct RetryAfterStore(DurationHintStore);

impl Default for RetryAfterStore {
    fn default() -> Self {
        Self(DurationHintStore::new("Retry-After"))
    }
}

impl RetryAfterStore {
    /// Create a new empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a Retry-After hint from a 429 response.
    pub fn record(&self, duration: Duration) {
        self.0.record(duration)
    }

    /// Take the stored hint if it exists and was recorded recently.
    pub fn take(&self, max_age: Duration) -> Option<Duration> {
        self.0.take(max_age)
    }
}

/// A shared store for gRPC retry pushback hints.
///
/// It passes `grpc-retry-pushback-ms` hints from the response path to the
/// circuit breaker and wraps [`DurationHintStore`] with the
/// "grpc-retry-pushback-ms" name for logging.
#[derive(Clone, Debug)]
pub struct GrpcRetryPushbackStore(DurationHintStore);

impl Default for GrpcRetryPushbackStore {
    fn default() -> Self {
        Self(DurationHintStore::new("grpc-retry-pushback-ms"))
    }
}

impl GrpcRetryPushbackStore {
    /// Create a new empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a grpc-retry-pushback-ms hint from a RESOURCE_EXHAUSTED response.
    pub fn record(&self, duration: Duration) {
        self.0.record(duration)
    }

    /// Take the stored hint if it exists and was recorded recently.
    pub fn take(&self, max_age: Duration) -> Option<Duration> {
        self.0.take(max_age)
    }
}

/// Parse grpc-retry-pushback-ms from headers or trailers when RESOURCE_EXHAUSTED.
///
/// It delegates to the shared parser in [`linkerd_http_classify::retry_after`]
/// only when `grpc_code` is RESOURCE_EXHAUSTED (code 8), and `max` caps the
/// parsed duration at the probe backoff's maximum. Any other code or a parse
/// failure returns `None`.
fn gated_parse_grpc_retry_pushback(
    grpc_code: Option<Code>,
    headers_or_trailers: &HeaderMap,
    max: Duration,
) -> Option<Duration> {
    if grpc_code != Some(Code::ResourceExhausted) {
        return None;
    }
    linkerd_http_classify::retry_after::parse_grpc_retry_pushback(headers_or_trailers, max)
}

/// A ClassifyEos wrapper that records grpc-retry-pushback-ms hints from trailers.
///
/// It wraps any [`ClassifyEos`] implementation and intercepts the `eos()` call
/// to parse `grpc-retry-pushback-ms` trailers from RESOURCE_EXHAUSTED responses,
/// recording the parsed duration in a [`GrpcRetryPushbackStore`] for the circuit
/// breaker's backoff logic, while all classification work is delegated to the
/// inner classifier unchanged.
#[derive(Clone, Debug)]
pub struct GrpcRetryPushbackClassifyEos<E> {
    inner: E,
    store: GrpcRetryPushbackStore,
    max_duration: Duration,
    /// Whether trailer parsing runs. The breaker reads the store only when it
    /// respects hints, so an endpoint without that opt-in carries a disabled
    /// wrapper that delegates classification and parses nothing.
    record: bool,
    /// Whether the response head was HTTP 200 OK, decided in `start` and carried
    /// here. gRPC trailers are only meaningful on a 200 response, so a
    /// `grpc-retry-pushback-ms` trailer on a non-200 response must not be
    /// recorded, the same gate the header path applies in `start`.
    is_ok_status: bool,
}

impl<E> GrpcRetryPushbackClassifyEos<E> {
    /// Create a new wrapper around the given ClassifyEos. `is_ok_status` carries
    /// whether the response head was HTTP 200 OK so trailer parsing can match the
    /// header path's 200-only gate.
    pub fn new(
        inner: E,
        store: GrpcRetryPushbackStore,
        max_duration: Duration,
        is_ok_status: bool,
    ) -> Self {
        Self {
            inner,
            store,
            max_duration,
            record: true,
            is_ok_status,
        }
    }

    /// Create a wrapper that delegates classification but never parses or records
    /// a trailer hint, so a non-hint endpoint shares the same classifier type
    /// without paying for parsing the breaker would never read.
    pub fn disabled(inner: E, store: GrpcRetryPushbackStore, max_duration: Duration) -> Self {
        Self {
            inner,
            store,
            max_duration,
            record: false,
            is_ok_status: false,
        }
    }
}

impl<E> ClassifyEos for GrpcRetryPushbackClassifyEos<E>
where
    E: ClassifyEos<Class = classify::Class>,
{
    type Class = classify::Class;

    fn eos(self, trailers: Option<&HeaderMap>) -> Self::Class {
        // Get classification from inner first
        let class = self.inner.eos(trailers);

        // Parse a trailer hint only when this endpoint records and the response
        // head was 200 OK. A non-200 response is never gRPC, so its trailers
        // cannot carry a meaningful gRPC pushback, matching the 200-only gate the
        // header path applies in `start`.
        if let (true, true, Some(trls)) = (self.record, self.is_ok_status, trailers) {
            // Extract gRPC code from the classification result
            let grpc_code = match &class {
                classify::Class::Grpc(Ok(code)) | classify::Class::Grpc(Err(code)) => Some(*code),
                _ => None,
            };

            if let Some(duration) =
                gated_parse_grpc_retry_pushback(grpc_code, trls, self.max_duration)
            {
                self.store.record(duration);
            }
        }

        class
    }

    fn error(self, error: &linkerd_app_core::Error) -> Self::Class {
        // Delegate error classification unchanged
        self.inner.error(error)
    }
}

// === take_combined_hint ===

/// Drain both hint stores and return a single backoff floor clamped to the
/// probe backoff's bounds.
///
/// Both the consecutive-failures and unified breakers derive their
/// Retry-After/pushback floor here, so the clamp and drain rules stay identical
/// between them. A hint is honored only when it is no older than `max`, and the
/// store already subtracts elapsed time so the remaining wait reflects how much
/// of the hint is still in the future.
///
/// Each taken hint is clamped into `[min, max]`, the probe backoff's minimum and
/// maximum durations. One response then cannot push the breaker past the backoff
/// it would otherwise escalate to, and a hint shorter than the base backoff
/// cannot shorten it. When both sources hold a hint the larger of the two clamped
/// values wins, and per-source tracing reports which hint was clamped and which
/// one was used.
pub(super) fn take_combined_hint(
    http_store: &RetryAfterStore,
    grpc_store: &GrpcRetryPushbackStore,
    min: Duration,
    max: Duration,
) -> Option<Duration> {
    // `take(max)` discards hints older than the maximum backoff and clears the
    // slot, keeping the store contract that drops an old hint and consumes the
    // value on read.
    let http_hint = http_store
        .take(max)
        .map(|h| clamp_hint(h, min, max, "Retry-After"));
    let grpc_hint = grpc_store
        .take(max)
        .map(|g| clamp_hint(g, min, max, "grpc-retry-pushback-ms"));

    match (http_hint, grpc_hint) {
        (Some(h), Some(g)) => {
            let combined = h.max(g);
            tracing::debug!(
                http_hint = ?h,
                grpc_hint = ?g,
                combined = ?combined,
                "Combined HTTP and gRPC hints (max-value-wins)",
            );
            Some(combined)
        }
        (Some(h), None) => {
            tracing::debug!(hint = ?h, "Using HTTP Retry-After hint");
            Some(h)
        }
        (None, Some(g)) => {
            tracing::debug!(hint = ?g, "Using gRPC pushback hint");
            Some(g)
        }
        (None, None) => None,
    }
}

/// Clamp a single hint into the probe backoff's `[min, max]` bounds, tracing
/// when either bound bites so operators can see which source was adjusted.
fn clamp_hint(hint: Duration, min: Duration, max: Duration, source: &'static str) -> Duration {
    // A misconfigured `min > max` leaves no valid range, so collapse `min` to the
    // ceiling and keep the result at or below `max`, which the caller asserts as the
    // backoff bound, instead of emitting a value above it.
    let min = min.min(max);
    if hint > max {
        tracing::warn!(?hint, ?max, %source, "Hint exceeds maximum backoff, clamping to max");
        max
    } else if hint < min {
        tracing::debug!(?hint, ?min, %source, "Hint below minimum backoff, raising to min");
        min
    } else {
        hint
    }
}

// === RetryAfterClassify ===

/// A classifier wrapper that records retry hints from both HTTP and gRPC responses.
///
/// It wraps any [`ClassifyResponse`] implementation and intercepts the `start()`
/// call to parse `Retry-After` headers from HTTP 429 responses and
/// `grpc-retry-pushback-ms` from gRPC headers on unary failures, and to wrap the
/// inner `ClassifyEos` with [`GrpcRetryPushbackClassifyEos`] so gRPC trailers are
/// parsed too. The parsed durations are recorded in their respective stores for
/// the circuit breaker's backoff logic.
#[derive(Clone, Debug)]
pub struct RetryAfterClassify<C> {
    inner: C,
    http_store: RetryAfterStore,
    grpc_store: GrpcRetryPushbackStore,
    max_duration: Duration,
    /// Whether hint parsing runs. A disabled classifier delegates classification
    /// unchanged and records nothing, so every endpoint can share one classifier
    /// type while only a hint-respecting breaker pays the parsing cost.
    record: bool,
}

impl<C> RetryAfterClassify<C> {
    /// Test-only. Production code uses [`Self::new_with_max`].
    ///
    /// Create a new wrapper around the given classifier.
    #[cfg(test)]
    pub fn new(inner: C, http_store: RetryAfterStore, grpc_store: GrpcRetryPushbackStore) -> Self {
        Self {
            inner,
            http_store,
            grpc_store,
            max_duration: Duration::from_secs(300),
            record: true,
        }
    }

    /// Create a new wrapper with a custom max Retry-After cap.
    pub fn new_with_max(
        inner: C,
        http_store: RetryAfterStore,
        grpc_store: GrpcRetryPushbackStore,
        max_duration: Duration,
    ) -> Self {
        Self {
            inner,
            http_store,
            grpc_store,
            max_duration,
            record: true,
        }
    }

    /// Wrap a classifier so it delegates classification but parses and records no
    /// hints. An endpoint whose breaker ignores server hints carries this so it
    /// shares the recording classifier's type without doing parsing the breaker
    /// would never read, and an endpoint with no breaker carries it too.
    pub fn disabled(inner: C) -> Self {
        Self {
            inner,
            http_store: RetryAfterStore::new(),
            grpc_store: GrpcRetryPushbackStore::new(),
            max_duration: Duration::ZERO,
            record: false,
        }
    }
}

impl<C> ClassifyResponse for RetryAfterClassify<C>
where
    C: ClassifyResponse<Class = classify::Class, ClassifyEos = classify::Eos>,
{
    type Class = classify::Class;
    type ClassifyEos = GrpcRetryPushbackClassifyEos<classify::Eos>;

    fn start<B>(self, rsp: &http::Response<B>) -> Self::ClassifyEos {
        let max = self.max_duration;

        // A disabled classifier parses nothing and hands back a disabled trailer
        // wrapper, so a non-hint or breaker-free endpoint keeps this classifier's
        // type while doing no work the breaker would never observe.
        if !self.record {
            let inner_eos = self.inner.start(rsp);
            return GrpcRetryPushbackClassifyEos::disabled(inner_eos, self.grpc_store, max);
        }

        // Record a genuine HTTP Retry-After header into the HTTP store, while
        // gRPC pushback is recorded into its own store below, so the two hint
        // sources stay distinct and the "Retry-After" store never holds a value
        // that did not come from a Retry-After header.
        if let Some(duration) =
            linkerd_http_classify::retry_after::parse_retry_after(rsp.status(), rsp.headers(), max)
        {
            self.http_store.record(duration);
        }

        // gRPC headers (grpc-status, grpc-retry-pushback-ms) are only meaningful
        // when HTTP status is 200 OK. Non-200 responses are never gRPC. The same
        // gate carries to the trailer path through the eos wrapper below.
        let is_ok_status = rsp.status() == http::StatusCode::OK;
        if is_ok_status {
            let grpc_code = grpc_code(rsp.headers());
            if let Some(duration) = gated_parse_grpc_retry_pushback(grpc_code, rsp.headers(), max) {
                self.grpc_store.record(duration);
            }
        }

        // Wrap the inner ClassifyEos with GrpcRetryPushbackClassifyEos to
        // also parse gRPC trailers when the body ends, gated on the same 200-only
        // status so a trailer hint on a non-200 response is not recorded.
        let inner_eos = self.inner.start(rsp);
        GrpcRetryPushbackClassifyEos::new(inner_eos, self.grpc_store, max, is_ok_status)
    }

    fn error(self, error: &linkerd_app_core::Error) -> Self::Class {
        // Delegate error classification unchanged.
        self.inner.error(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use linkerd_proxy_client_policy::http::StatusRanges;

    /// A simplified local model where the largest value wins, which keeps the
    /// store-level unit tests self-contained. It leaves both inputs unclamped
    /// because the clamp to the backoff window in [`take_combined_hint`] is
    /// pinned in full against the real code in the breaker integration tests.
    fn combine_hints(http: Option<Duration>, grpc: Option<Duration>) -> Option<Duration> {
        match (http, grpc) {
            (Some(h), Some(g)) => Some(h.max(g)),
            (h @ Some(_), None) => h,
            (None, g @ Some(_)) => g,
            (None, None) => None,
        }
    }

    // === RetryAfterStore tests ===

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn store_last_value_wins() {
        let store = RetryAfterStore::new();

        // Record a 10-second hint.
        store.record(Duration::from_secs(10));

        // A fresher, smaller hint replaces it. A recovering server that lowers
        // its Retry-After is honored at the lower value, and the earlier larger
        // value no longer pins it.
        store.record(Duration::from_secs(5));

        // The stored value is the freshest one (5 seconds).
        let hint = store.take(Duration::from_secs(1));
        assert_eq!(hint, Some(Duration::from_secs(5)));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn store_larger_value_updates() {
        let store = RetryAfterStore::new();

        // Record a 5-second hint
        store.record(Duration::from_secs(5));

        // Record a larger hint of 10 seconds, which updates the stored value.
        store.record(Duration::from_secs(10));

        // The stored value should be 10 seconds
        let hint = store.take(Duration::from_secs(1));
        assert_eq!(hint, Some(Duration::from_secs(10)));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn store_empty_accepts_any() {
        let store = RetryAfterStore::new();

        // Empty store should accept any value
        store.record(Duration::from_secs(5));

        let hint = store.take(Duration::from_secs(1));
        assert_eq!(hint, Some(Duration::from_secs(5)));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn store_take_clears() {
        let store = RetryAfterStore::new();

        store.record(Duration::from_secs(10));

        // First take should succeed
        let hint1 = store.take(Duration::from_secs(1));
        assert_eq!(hint1, Some(Duration::from_secs(10)));

        // Second take should return None (consumed)
        let hint2 = store.take(Duration::from_secs(1));
        assert_eq!(hint2, None);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn store_after_take_accepts_new() {
        let store = RetryAfterStore::new();

        // Record and take
        store.record(Duration::from_secs(10));
        let _ = store.take(Duration::from_secs(1));

        // Now any new value should be accepted (store is empty)
        store.record(Duration::from_secs(5));

        let hint = store.take(Duration::from_secs(1));
        assert_eq!(hint, Some(Duration::from_secs(5)));
    }

    #[test]
    fn parse_grpc_pushback_positive() {
        let max = Duration::from_secs(300);
        let mut headers = HeaderMap::new();
        headers.insert("grpc-retry-pushback-ms", HeaderValue::from_static("5000"));

        let result = gated_parse_grpc_retry_pushback(Some(Code::ResourceExhausted), &headers, max);
        assert_eq!(result, Some(Duration::from_millis(5000)));
    }

    #[test]
    fn parse_grpc_pushback_zero() {
        let max = Duration::from_secs(300);
        let mut headers = HeaderMap::new();
        headers.insert("grpc-retry-pushback-ms", HeaderValue::from_static("0"));

        let result = gated_parse_grpc_retry_pushback(Some(Code::ResourceExhausted), &headers, max);
        assert_eq!(result, Some(Duration::ZERO));
    }

    #[test]
    fn parse_grpc_pushback_negative_ignored() {
        let max = Duration::from_secs(300);
        let mut headers = HeaderMap::new();
        headers.insert("grpc-retry-pushback-ms", HeaderValue::from_static("-1"));

        let result = gated_parse_grpc_retry_pushback(Some(Code::ResourceExhausted), &headers, max);
        // Negative values mean "do not retry", so we ignore them.
        assert_eq!(result, None);
    }

    #[test]
    fn parse_grpc_pushback_non_resource_exhausted_ignored() {
        let max = Duration::from_secs(300);
        let mut headers = HeaderMap::new();
        headers.insert("grpc-retry-pushback-ms", HeaderValue::from_static("5000"));

        let result = gated_parse_grpc_retry_pushback(Some(Code::Ok), &headers, max);
        assert_eq!(result, None);

        let result = gated_parse_grpc_retry_pushback(Some(Code::Internal), &headers, max);
        assert_eq!(result, None);

        let result = gated_parse_grpc_retry_pushback(Some(Code::Unavailable), &headers, max);
        assert_eq!(result, None);

        // None (no code)
        let result = gated_parse_grpc_retry_pushback(None, &headers, max);
        assert_eq!(result, None);
    }

    #[test]
    fn parse_grpc_pushback_caps_at_max() {
        let max = Duration::from_secs(300);
        let mut headers = HeaderMap::new();
        // 999999999 ms should be capped at max (300s)
        headers.insert(
            "grpc-retry-pushback-ms",
            HeaderValue::from_static("999999999"),
        );

        let result = gated_parse_grpc_retry_pushback(Some(Code::ResourceExhausted), &headers, max);
        assert_eq!(result, Some(max));
    }

    #[test]
    fn parse_grpc_pushback_invalid_format() {
        let max = Duration::from_secs(300);
        let mut headers = HeaderMap::new();
        headers.insert("grpc-retry-pushback-ms", HeaderValue::from_static("abc"));

        let result = gated_parse_grpc_retry_pushback(Some(Code::ResourceExhausted), &headers, max);
        assert_eq!(result, None);
    }

    #[test]
    fn parse_grpc_pushback_missing_header() {
        let max = Duration::from_secs(300);
        let headers = HeaderMap::new();

        let result = gated_parse_grpc_retry_pushback(Some(Code::ResourceExhausted), &headers, max);
        assert_eq!(result, None);
    }

    #[test]
    fn parse_grpc_pushback_empty_value() {
        let max = Duration::from_secs(300);
        let mut headers = HeaderMap::new();
        headers.insert("grpc-retry-pushback-ms", HeaderValue::from_static(""));

        let result = gated_parse_grpc_retry_pushback(Some(Code::ResourceExhausted), &headers, max);
        assert_eq!(result, None);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn grpc_store_last_value_wins() {
        let store = GrpcRetryPushbackStore::new();

        // Record a 10-second hint.
        store.record(Duration::from_secs(10));

        // A fresher, smaller hint replaces it. A recovering server that lowers
        // its pushback is honored at the lower value, and the earlier larger
        // value no longer pins it.
        store.record(Duration::from_secs(5));

        // The stored value is the freshest one (5 seconds).
        let hint = store.take(Duration::from_secs(1));
        assert_eq!(hint, Some(Duration::from_secs(5)));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn grpc_store_larger_value_updates() {
        let store = GrpcRetryPushbackStore::new();

        // Record a 5-second hint
        store.record(Duration::from_secs(5));

        // Record a larger hint of 10 seconds, which updates the stored value.
        store.record(Duration::from_secs(10));

        // The stored value should be 10 seconds
        let hint = store.take(Duration::from_secs(1));
        assert_eq!(hint, Some(Duration::from_secs(10)));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn grpc_store_take_clears() {
        let store = GrpcRetryPushbackStore::new();

        store.record(Duration::from_secs(10));

        // First take should succeed
        let hint1 = store.take(Duration::from_secs(1));
        assert_eq!(hint1, Some(Duration::from_secs(10)));

        // Second take should return None (consumed)
        let hint2 = store.take(Duration::from_secs(1));
        assert_eq!(hint2, None);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn combined_http_and_grpc_hints_max_wins() {
        let http_store = RetryAfterStore::new();
        let grpc_store = GrpcRetryPushbackStore::new();
        let max_age = Duration::from_secs(60);

        // Record HTTP hint of 10 seconds
        http_store.record(Duration::from_secs(10));
        // Record gRPC hint of 30 seconds (larger, so it wins)
        grpc_store.record(Duration::from_secs(30));

        let http_hint = http_store.take(max_age);
        let grpc_hint = grpc_store.take(max_age);

        // Verify both hints are retrieved
        assert_eq!(http_hint, Some(Duration::from_secs(10)));
        assert_eq!(grpc_hint, Some(Duration::from_secs(30)));

        // The larger of the two hints wins.
        let effective = combine_hints(http_hint, grpc_hint);

        assert_eq!(effective, Some(Duration::from_secs(30)));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn combined_hints_http_wins_when_larger() {
        let http_store = RetryAfterStore::new();
        let grpc_store = GrpcRetryPushbackStore::new();
        let max_age = Duration::from_secs(60);

        // Record HTTP hint of 60 seconds (larger)
        http_store.record(Duration::from_secs(60));
        // Record gRPC hint of 15 seconds
        grpc_store.record(Duration::from_secs(15));

        let http_hint = http_store.take(max_age);
        let grpc_hint = grpc_store.take(max_age);

        let effective = combine_hints(http_hint, grpc_hint);

        assert_eq!(effective, Some(Duration::from_secs(60)));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn combined_hints_only_http() {
        let http_store = RetryAfterStore::new();
        let grpc_store = GrpcRetryPushbackStore::new();
        let max_age = Duration::from_secs(60);

        // Record only HTTP hint
        http_store.record(Duration::from_secs(20));

        let http_hint = http_store.take(max_age);
        let grpc_hint = grpc_store.take(max_age);

        let effective = combine_hints(http_hint, grpc_hint);

        assert_eq!(effective, Some(Duration::from_secs(20)));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn combined_hints_only_grpc() {
        let http_store = RetryAfterStore::new();
        let grpc_store = GrpcRetryPushbackStore::new();
        let max_age = Duration::from_secs(60);

        // Record only gRPC hint
        grpc_store.record(Duration::from_secs(25));

        let http_hint = http_store.take(max_age);
        let grpc_hint = grpc_store.take(max_age);

        let effective = combine_hints(http_hint, grpc_hint);

        assert_eq!(effective, Some(Duration::from_secs(25)));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn stale_hint_discarded() {
        let store = DurationHintStore::new("test");
        store.record(Duration::from_secs(5));

        // Advance time past the max_age
        tokio::time::advance(Duration::from_secs(60)).await;

        // An old hint must be dropped here rather than returned.
        let result = store.take(Duration::from_secs(30));
        assert!(result.is_none(), "Stale hint should be discarded");

        // Store should be empty now (old hint was cleared)
        let result2 = store.take(Duration::from_secs(30));
        assert!(
            result2.is_none(),
            "Store should be empty after stale discard"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn take_adjusts_for_elapsed_time() {
        let store = DurationHintStore::new("test");

        // Record a 10-second hint
        store.record(Duration::from_secs(10));

        // Advance 3 seconds. The hint is partly elapsed.
        tokio::time::advance(Duration::from_secs(3)).await;

        // take() should return 10s - 3s = 7s (elapsed < duration)
        let result = store.take(Duration::from_secs(60));
        assert_eq!(result, Some(Duration::from_secs(7)));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn exactly_elapsed_hint_returns_none() {
        let store = DurationHintStore::new("test");

        // Record a 5-second hint
        store.record(Duration::from_secs(5));

        // Advance exactly 5 seconds, so elapsed equals duration.
        tokio::time::advance(Duration::from_secs(5)).await;

        // take() should return None (elapsed >= duration)
        let result = store.take(Duration::from_secs(60));
        assert_eq!(result, None);

        // The store is now empty since the take consumed the hint.
        assert_eq!(store.take(Duration::from_secs(60)), None);

        // The hint was consumed above: the second take() returned None. A fresh
        // record is then honored at its full duration.
        store.record(Duration::from_secs(3));
        assert_eq!(
            store.take(Duration::from_secs(60)),
            Some(Duration::from_secs(3))
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn overshot_hint_returns_none() {
        let store = DurationHintStore::new("test");

        // Record a 2-second hint
        store.record(Duration::from_secs(2));

        // Advance 3 seconds, so elapsed passes duration but stays within max_age.
        tokio::time::advance(Duration::from_secs(3)).await;

        // take() should return None (elapsed > duration)
        let result = store.take(Duration::from_secs(60));
        assert_eq!(result, None);

        // The store is now empty since the take consumed the hint.
        assert_eq!(store.take(Duration::from_secs(60)), None);

        // The hint was consumed above: the second take() returned None. A fresh
        // record is then honored at its full duration.
        store.record(Duration::from_secs(1));
        assert_eq!(
            store.take(Duration::from_secs(60)),
            Some(Duration::from_secs(1))
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn repeated_equal_hint_refreshes_age() {
        let store = DurationHintStore::new("test");

        // A server emits the same 10-second pushback twice, six seconds apart.
        store.record(Duration::from_secs(10));
        tokio::time::advance(Duration::from_secs(6)).await;
        store.record(Duration::from_secs(10));

        // Six more seconds pass: twelve since the first hint, six since the
        // latest.
        tokio::time::advance(Duration::from_secs(6)).await;

        // Age is measured from the latest hint, so the pushback is still live and
        // take() returns the remaining wait (10 - 6 = 4 seconds). If the timestamp
        // had stayed frozen at first arrival, twelve seconds would have elapsed
        // and the hint would be discarded as fully elapsed.
        let hint = store.take(Duration::from_secs(60));
        assert_eq!(hint, Some(Duration::from_secs(4)));
    }

    #[test]
    fn combined_hints_neither_present() {
        let http_store = RetryAfterStore::new();
        let grpc_store = GrpcRetryPushbackStore::new();
        let max_age = Duration::from_secs(60);

        let http_hint = http_store.take(max_age);
        let grpc_hint = grpc_store.take(max_age);

        let effective = combine_hints(http_hint, grpc_hint);

        assert_eq!(effective, None);
    }

    #[test]
    fn clamp_hint_inverted_bounds_stay_within_max() {
        // A misconfigured backoff with min > max leaves no valid range, so the
        // result must never exceed max because the breaker asserts max as the
        // backoff ceiling. Before the guard, a hint below min returned min, which
        // is above max here.
        let min = Duration::from_secs(10);
        let max = Duration::from_secs(5);
        for hint in [
            Duration::from_secs(3),
            Duration::from_secs(7),
            Duration::from_secs(20),
        ] {
            assert!(clamp_hint(hint, min, max, "test") <= max);
        }
    }

    // grpc-status must stay unparsed on a non-200 response, so an HTTP 429 with a
    // spurious `grpc-status: 8` header records the Retry-After hint in the HTTP
    // store and leaves the gRPC store empty.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn grpc_status_skipped_on_non_200() {
        let http_store = RetryAfterStore::new();
        let grpc_store = GrpcRetryPushbackStore::new();
        let inner = classify::Response::Http(StatusRanges::default());
        let wrapped = RetryAfterClassify::new(inner, http_store.clone(), grpc_store.clone());

        // HTTP 429 with Retry-After AND a spurious grpc-status: 8 header.
        let mut rsp = http::Response::new(());
        *rsp.status_mut() = http::StatusCode::TOO_MANY_REQUESTS;
        rsp.headers_mut().insert(
            http::header::RETRY_AFTER,
            http::HeaderValue::from_static("30"),
        );
        rsp.headers_mut()
            .insert("grpc-status", http::HeaderValue::from_static("8"));
        rsp.headers_mut().insert(
            "grpc-retry-pushback-ms",
            http::HeaderValue::from_static("5000"),
        );

        let _eos = wrapped.start(&rsp);

        // HTTP store should have the Retry-After hint
        let http_hint = http_store.take(Duration::from_secs(10));
        assert_eq!(http_hint, Some(Duration::from_secs(30)));

        // gRPC store should be empty
        let grpc_hint = grpc_store.take(Duration::from_secs(10));
        assert_eq!(
            grpc_hint, None,
            "grpc-status should not be parsed on HTTP 429"
        );
    }

    // 200 OK gRPC pushback must land in the gRPC store alone, so a 200 OK with
    // `grpc-status: 8` (RESOURCE_EXHAUSTED) and `grpc-retry-pushback-ms: 5000`
    // records grpc_store = Some(5s) and leaves http_store empty. There is no HTTP
    // Retry-After header, so the "Retry-After" store must not pick up the gRPC
    // pushback.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn grpc_pushback_recorded_only_in_grpc_store() {
        let http_store = RetryAfterStore::new();
        let grpc_store = GrpcRetryPushbackStore::new();
        let inner = classify::Response::Http(StatusRanges::default());
        let wrapped = RetryAfterClassify::new(inner, http_store.clone(), grpc_store.clone());

        // HTTP 200 OK with gRPC RESOURCE_EXHAUSTED + pushback header.
        let mut rsp = http::Response::new(());
        *rsp.status_mut() = http::StatusCode::OK;
        rsp.headers_mut().insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/grpc"),
        );
        rsp.headers_mut()
            .insert("grpc-status", http::HeaderValue::from_static("8"));
        rsp.headers_mut().insert(
            "grpc-retry-pushback-ms",
            http::HeaderValue::from_static("5000"),
        );

        let _eos = wrapped.start(&rsp);

        // gRPC store should have the pushback hint (the check let it through).
        let grpc_hint = grpc_store.take(Duration::from_secs(10));
        assert_eq!(
            grpc_hint,
            Some(Duration::from_millis(5000)),
            "200 OK with grpc-status: 8 should record gRPC pushback"
        );

        // HTTP store stays empty. No Retry-After header was present, and the
        // gRPC pushback belongs only to the gRPC store.
        let http_hint = http_store.take(Duration::from_secs(10));
        assert_eq!(
            http_hint, None,
            "gRPC pushback must not leak into the Retry-After store"
        );
    }

    // A gRPC pushback trailer on a non-200 response must not be recorded, the
    // same gate the header path applies. The status comes in the head as a
    // non-200, the grpc-status and pushback arrive only in trailers, and the
    // store must stay empty because a non-200 response is never gRPC.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn grpc_trailer_pushback_skipped_on_non_200() {
        let http_store = RetryAfterStore::new();
        let grpc_store = GrpcRetryPushbackStore::new();
        let inner = classify::Response::Grpc(Default::default());
        let wrapped = RetryAfterClassify::new(inner, http_store.clone(), grpc_store.clone());

        // Non-200 head with no grpc-status header, so classification stays open
        // until the trailers arrive.
        let mut rsp = http::Response::new(());
        *rsp.status_mut() = http::StatusCode::SERVICE_UNAVAILABLE;
        let eos = wrapped.start(&rsp);

        // Trailers carry RESOURCE_EXHAUSTED and a pushback, which on a 200 head
        // would record but here must not.
        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", HeaderValue::from_static("8"));
        trailers.insert("grpc-retry-pushback-ms", HeaderValue::from_static("5000"));
        let _class = eos.eos(Some(&trailers));

        let grpc_hint = grpc_store.take(Duration::from_secs(10));
        assert_eq!(
            grpc_hint, None,
            "a gRPC pushback trailer on a non-200 response must not be recorded",
        );
    }

    // The 200-OK companion to the non-200 trailer case: with a 200 head the same
    // RESOURCE_EXHAUSTED pushback trailer is recorded, so the new status gate
    // narrows only the non-200 path and leaves the live trailer path intact.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn grpc_trailer_pushback_recorded_on_200() {
        let http_store = RetryAfterStore::new();
        let grpc_store = GrpcRetryPushbackStore::new();
        let inner = classify::Response::Grpc(Default::default());
        let wrapped = RetryAfterClassify::new(inner, http_store.clone(), grpc_store.clone());

        let mut rsp = http::Response::new(());
        *rsp.status_mut() = http::StatusCode::OK;
        let eos = wrapped.start(&rsp);

        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", HeaderValue::from_static("8"));
        trailers.insert("grpc-retry-pushback-ms", HeaderValue::from_static("5000"));
        let _class = eos.eos(Some(&trailers));

        let grpc_hint = grpc_store.take(Duration::from_secs(10));
        assert_eq!(
            grpc_hint,
            Some(Duration::from_millis(5000)),
            "a gRPC pushback trailer on a 200 response is recorded",
        );
    }
}
