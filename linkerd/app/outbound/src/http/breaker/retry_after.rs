//! Retry-After and grpc-retry-pushback-ms parsing for rate limiting responses.
//!
//! Parses backoff hints from HTTP 429/503 and gRPC RESOURCE_EXHAUSTED responses
//! into [`RetryAfterStore`] and [`GrpcRetryPushbackStore`]. Both use a
//! max-value-wins strategy (longest hint kept). The circuit breaker queries
//! both via `UnifiedBreaker::take_combined_hint()` and uses the maximum.
//!
//! Parsing is delegated to [`linkerd_http_classify::retry_after`].
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
//! UnifiedBreaker::take_combined_hint()
//!     |
//!     +-> RetryAfterStore::take()
//!     +-> GrpcRetryPushbackStore::take()
//!             |
//!             v
//!         max(http_hint, grpc_hint)
//! ```

use http::HeaderMap;
use linkerd_app_core::classify::{self, grpc_code};
use linkerd_http_classify::{ClassifyEos, ClassifyResponse};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tonic::Code;

/// Shared store for duration hints, timestamped to detect staleness.
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

    /// Record a duration hint (max value wins, only updates if longer).
    pub fn record(&self, duration: Duration) {
        let mut guard = self.inner.lock();
        let should_update = match *guard {
            None => true,
            Some((_, existing)) => duration > existing,
        };

        if should_update {
            *guard = Some((Instant::now(), duration));
            tracing::debug!(
                ?duration,
                hint_type = %self.name,
                "Recorded hint (max value wins)",
            );
        } else {
            tracing::debug!(
                ?duration,
                existing = ?guard.as_ref().map(|(_, d)| d),
                hint_type = %self.name,
                "Ignoring smaller hint",
            );
        }
    }

    /// Take the stored hint if it exists and was recorded recently.
    ///
    /// The hint is consumed (set to None) to prevent reusing the same hint
    /// for multiple backoff cycles. The `max_age` parameter controls how
    /// old a hint can be before it's considered stale.
    ///
    /// Stale hints are discarded without being consumed: we inspect the
    /// timestamp first, then only `take()` if the hint is recent. This
    /// prevents a race where a stale hint is consumed (clearing the store)
    /// when a new hint arrives.
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

            // Stale. Discard explicitly without consuming for use
            *guard = None;
        }
        None
    }
}

/// A shared store for Retry-After hints.
///
/// This is used to pass Retry-After hints from the response path to the circuit
/// breaker. Wraps [`DurationHintStore`] with the "Retry-After" name for logging.
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
/// This is used to pass `grpc-retry-pushback-ms` hints from the response path
/// to the circuit breaker. Wraps [`DurationHintStore`] with the
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

/// Parse grpc-retry-pushback-ms from headers or trailers if code is RESOURCE_EXHAUSTED.
///
/// Delegates to the shared parser in [`linkerd_http_classify::retry_after`],
/// but only when `grpc_code` is RESOURCE_EXHAUSTED (code 8).
///
/// `max` caps the parsed duration (typically `max_duration` from config).
///
/// Returns `None` for non-RESOURCE_EXHAUSTED responses or any parse failure.
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
/// This wraps any [`ClassifyEos`] implementation and intercepts the `eos()`
/// call to parse `grpc-retry-pushback-ms` trailers from RESOURCE_EXHAUSTED
/// responses. The parsed duration is recorded in a [`GrpcRetryPushbackStore`]
/// for use by the circuit breaker's backoff logic.
///
/// The wrapper delegates all classification logic to the inner classifier unchanged.
#[derive(Clone, Debug)]
pub struct GrpcRetryPushbackClassifyEos<E> {
    inner: E,
    store: GrpcRetryPushbackStore,
    max_duration: Duration,
}

impl<E> GrpcRetryPushbackClassifyEos<E> {
    /// Create a new wrapper around the given ClassifyEos.
    pub fn new(inner: E, store: GrpcRetryPushbackStore, max_duration: Duration) -> Self {
        Self {
            inner,
            store,
            max_duration,
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

        // If we have trailers, check for grpc-retry-pushback-ms
        if let Some(trls) = trailers {
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

// === RetryAfterClassify ===

/// A classifier wrapper that records retry hints from both HTTP and gRPC responses.
///
/// This wraps any [`ClassifyResponse`] implementation and intercepts the `start()`
/// call to:
/// 1. Parse `Retry-After` headers from HTTP 429 responses
/// 2. Parse `grpc-retry-pushback-ms` from gRPC headers (for unary failures)
/// 3. Wrap the inner `ClassifyEos` with [`GrpcRetryPushbackClassifyEos`] to
///    also parse gRPC trailers
///
/// The parsed durations are recorded in their respective stores for use by the
/// circuit breaker's backoff logic.
#[derive(Clone, Debug)]
pub struct RetryAfterClassify<C> {
    inner: C,
    http_store: RetryAfterStore,
    grpc_store: GrpcRetryPushbackStore,
    max_duration: Duration,
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
        // Record only a genuine HTTP Retry-After header into the HTTP store.
        // gRPC pushback is recorded on its own into its store below, so the
        // two hint sources stay distinct and the "Retry-After" store never holds
        // a value that did not come from a Retry-After header.
        let max = self.max_duration;
        if let Some(duration) =
            linkerd_http_classify::retry_after::parse_retry_after(rsp.status(), rsp.headers(), max)
        {
            self.http_store.record(duration);
        }

        // gRPC headers (grpc-status, grpc-retry-pushback-ms) are only meaningful
        // when HTTP status is 200 OK. Non-200 responses are never gRPC.
        if rsp.status() == http::StatusCode::OK {
            let grpc_code = grpc_code(rsp.headers());
            if let Some(duration) = gated_parse_grpc_retry_pushback(grpc_code, rsp.headers(), max) {
                self.grpc_store.record(duration);
            }
        }

        // Wrap the inner ClassifyEos with GrpcRetryPushbackClassifyEos to
        // also parse gRPC trailers when the body ends.
        let inner_eos = self.inner.start(rsp);
        GrpcRetryPushbackClassifyEos::new(inner_eos, self.grpc_store, max)
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

    /// A simplified local model of max-value-wins combination, used to keep the
    /// store-level unit tests self-contained. Unlike the breaker's
    /// `UnifiedBreaker::take_combined_hint`, it does not clamp either input to a
    /// maximum duration. The clamp is pinned end-to-end against the real code in
    /// the breaker integration tests.
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
    async fn store_max_value_wins() {
        let store = RetryAfterStore::new();

        // Record a 10-second hint
        store.record(Duration::from_secs(10));

        // Try to record a smaller hint (5 seconds) - should be ignored
        store.record(Duration::from_secs(5));

        // The stored value should still be 10 seconds
        let hint = store.take(Duration::from_secs(1));
        assert_eq!(hint, Some(Duration::from_secs(10)));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn store_larger_value_updates() {
        let store = RetryAfterStore::new();

        // Record a 5-second hint
        store.record(Duration::from_secs(5));

        // Record a larger hint (10 seconds) - should update
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
        // Negative values mean "do not retry" - we ignore them
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
    async fn grpc_store_max_value_wins() {
        let store = GrpcRetryPushbackStore::new();

        // Record a 10-second hint
        store.record(Duration::from_secs(10));

        // Try to record a smaller hint (5 seconds) - should be ignored
        store.record(Duration::from_secs(5));

        // The stored value should still be 10 seconds
        let hint = store.take(Duration::from_secs(1));
        assert_eq!(hint, Some(Duration::from_secs(10)));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn grpc_store_larger_value_updates() {
        let store = GrpcRetryPushbackStore::new();

        // Record a 5-second hint
        store.record(Duration::from_secs(5));

        // Record a larger hint (10 seconds) - should update
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
        // Record gRPC hint of 30 seconds (larger - should win)
        grpc_store.record(Duration::from_secs(30));

        let http_hint = http_store.take(max_age);
        let grpc_hint = grpc_store.take(max_age);

        // Verify both hints are retrieved
        assert_eq!(http_hint, Some(Duration::from_secs(10)));
        assert_eq!(grpc_hint, Some(Duration::from_secs(30)));

        // Verify max-value-wins logic (as implemented in UnifiedBreaker::take_combined_hint)
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

        // Stale hint should be discarded, not returned
        let result = store.take(Duration::from_secs(30));
        assert!(result.is_none(), "Stale hint should be discarded");

        // Store should be empty now (stale hint was cleared)
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

        // Store should be empty (hint consumed, not just ignored)
        assert_eq!(store.take(Duration::from_secs(60)), None);

        // Prove the slot was cleared: a new (smaller) hint is immediately visible.
        // If take() failed to clear the slot, record(3s) would be rejected by
        // max-value-wins (3 < 5), and this assertion would fail.
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

        // Store should be empty (hint consumed, not just ignored)
        assert_eq!(store.take(Duration::from_secs(60)), None);

        // Prove the slot was cleared: a new (smaller) hint is immediately visible.
        // If take() failed to clear the slot, record(1s) would be rejected by
        // max-value-wins (1 < 2), and this assertion would fail.
        store.record(Duration::from_secs(1));
        assert_eq!(
            store.take(Duration::from_secs(60)),
            Some(Duration::from_secs(1))
        );
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

    // Verify that grpc-status is not parsed on non-200 responses.
    //
    // An HTTP 429 with a spurious `grpc-status: 8` header should record
    // the Retry-After hint in the HTTP store but leave the gRPC store empty.
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

    // Verify that 200 OK gRPC pushback goes only into the gRPC store.
    //
    // A 200 OK with `grpc-status: 8` (RESOURCE_EXHAUSTED) and
    // `grpc-retry-pushback-ms: 5000` should record grpc_store = Some(5s) and
    // leave http_store empty. There is no HTTP Retry-After header, so the
    // "Retry-After" store must not pick up the gRPC pushback.
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
}
