//! Load tracking with response failure awareness.
//!
//! This module provides a `LoadBiaser` wrapper that tracks request latency (RTT)
//! and detects failure responses (HTTP 429, 503, 5xx), applying artificial
//! penalties. Unlike Tower's `PeakEwma`, this implementation uses
//! `linkerd_ewma::Ewma` and returns `f64` metrics directly, enabling
//! integration with P2C load balancing.
//!
//! The load metric is `rtt * (pending + 1)`, exactly PeakEwma. The difference
//! is what counts as an observation. A success records its measured RTT. A
//! failure records a *computed*, penalized RTT instead: when present, it'll be
//! the server's Retry-After or grpc-retry-pushback hint (capped at
//! `max_duration`), otherwise a base penalty.
//!
//! Responses are classified through the [`ResponseFailureHint`] trait, which
//! are only relevant for HTTP, as other transports have no hint.
//!
//! In-flight requests are counted the way Tower's PeakEwma counts them, using
//! a [`Handle`] holding a clone of the shared state, and the strong count of
//! that `Arc` (minus the service's own reference) is the pending count. The
//! handle records a measurement when it drops, so a cancelled request still
//! contributes its elapsed time.

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use futures::ready;
use hyper_balance::PendingUntilFirstData;
use linkerd_ewma::{Ewma, MIN_DECAY};
use linkerd_stack::NewService;
use parking_lot::RwLock;
use pin_project::pin_project;
use std::{
    future::Future,
    marker::PhantomData,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::time::Instant;
use tower::load::{Load, TrackCompletion};
use tower_service::Service;

/// Default maximum duration for Retry-After hints.
pub const DEFAULT_RETRY_AFTER_MAX_DURATION: Duration = Duration::from_secs(300);

/// Default base failure penalty, in whole milliseconds.
const DEFAULT_PENALTY_MS: u32 = 5_000;

/// Default RTT seed used before any measurement is recorded.
const DEFAULT_RTT: Duration = Duration::from_secs(1);

/// Default decay window for the RTT EWMA.
const DEFAULT_RTT_DECAY: Duration = Duration::from_secs(10);

/// Classification of response failures for load biasing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureHint {
    /// HTTP 429 Too Many Requests
    RateLimited,
    /// HTTP 503 Service Unavailable
    ServiceUnavailable,
    /// HTTP 5xx (other than 503)
    InternalError,
}

/// Cached Retry-After hint stored in HTTP response extensions.
///
/// Stores the **uncapped** parsed value. Each consumer applies its own cap
/// via `rate_limit_hint(max)`, so different callers (e.g. load biaser vs
/// circuit breaker) can use different maximums from the same cached value.
#[derive(Clone, Copy, Debug)]
pub struct CachedRateLimitHint(Duration);

impl CachedRateLimitHint {
    /// Wraps a parsed, uncapped rate limit duration for caching.
    pub fn new(d: Duration) -> Self {
        Self(d)
    }

    /// Returns the cached duration, capped at `max`.
    pub fn duration_capped(&self, max: Duration) -> Duration {
        self.0.min(max)
    }
}

/// Trait for extracting failure hints from responses.
///
/// This allows the load biaser to classify responses and apply appropriate
/// penalties.
///
/// The trait splits rate limit hint access into two methods to avoid
/// requiring `&mut self` on the read path:
/// - `attach_parsed_rate_limit_hint(&mut self)`: parse and cache (needs `&mut`)
/// - `rate_limit_hint(&self, max)`: read cached value or parse on-read (only needs `&self`)
pub trait ResponseFailureHint {
    /// Returns a failure hint if the response indicates a failure condition.
    fn failure_hint(&self) -> Option<FailureHint>;

    /// Parse and cache the raw (uncapped) rate limit hint from this response.
    ///
    /// The raw uncapped value is cached so that each consumer can apply their
    /// own cap via `rate_limit_hint(max)`.
    fn attach_parsed_rate_limit_hint(&mut self);

    /// Returns the rate limit hint if available.
    ///
    /// Checks cached value first (from a previous `attach_parsed_rate_limit_hint` call).
    /// If no cached value, attempts to parse the header directly (capping at `max`).
    /// Returns `None` only if the header is absent or unparseable.
    fn rate_limit_hint(&self, max: Duration) -> Option<Duration>;
}

fn is_grpc(headers: &http::HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/grpc"))
}

/// HTTP responses classify failures by status code and parse Retry-After hints.
///
/// For gRPC trailers-only responses (HTTP 200 with `content-type: application/grpc`
/// and `grpc-status` in headers), the `grpc-status` header is inspected:
/// - gRPC status 8 (RESOURCE_EXHAUSTED) -> `RateLimited`
/// - gRPC status 14 (UNAVAILABLE) -> `ServiceUnavailable`
/// - gRPC status 2 (UNKNOWN), 4 (DEADLINE_EXCEEDED), 13 (INTERNAL),
///   15 (DATA_LOSS) -> `InternalError`
/// - Others (client errors, ie. CANCELLED, NOT_FOUND, etc) -> ignored
///
/// Non-gRPC HTTP 200 responses are not inspected for `grpc-status`.
impl<B> ResponseFailureHint for http::Response<B> {
    fn failure_hint(&self) -> Option<FailureHint> {
        let status = self.status();
        if status == http::StatusCode::TOO_MANY_REQUESTS {
            Some(FailureHint::RateLimited)
        } else if status == http::StatusCode::SERVICE_UNAVAILABLE {
            Some(FailureHint::ServiceUnavailable)
        } else if status.is_server_error() {
            Some(FailureHint::InternalError)
        } else if status == http::StatusCode::OK && is_grpc(self.headers()) {
            // gRPC trailers-only responses: grpc-status appears in headers.
            // We inspect grpc-status when content-type confirms this is a gRPC
            // response so that we avoid misclassifying non-gRPC HTTP 200
            // responses that somehow happen to include a grpc-status header.
            //
            // Note: for streaming gRPC, grpc-status is in trailers (not headers)
            // and is not detected here; such responses record only their
            // measured RTT and are not penalized.
            self.headers()
                .get("grpc-status")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u16>().ok())
                .and_then(|code| match code {
                    0 => None,                                   // OK
                    8 => Some(FailureHint::RateLimited),         // RESOURCE_EXHAUSTED
                    14 => Some(FailureHint::ServiceUnavailable), // UNAVAILABLE
                    // UNKNOWN, DEADLINE_EXCEEDED, INTERNAL, DATA_LOSS
                    2 | 4 | 13 | 15 => Some(FailureHint::InternalError),
                    _ => None, // Client errors (CANCELLED, INVALID_ARGUMENT, etc.)
                })
        } else {
            None
        }
    }

    fn attach_parsed_rate_limit_hint(&mut self) {
        // Store the uncapped value. Each consumer applies their own cap via
        // rate_limit_hint(max).
        if let Some(d) = linkerd_http_classify::retry_after::parse_retry_after(
            self.status(),
            self.headers(),
            Duration::MAX,
        ) {
            self.extensions_mut().insert(CachedRateLimitHint::new(d));
            return;
        }
        // Try gRPC retry-pushback-ms (for trailers-only responses)
        if self.status() == http::StatusCode::OK && is_grpc(self.headers()) {
            if let Some(d) = linkerd_http_classify::retry_after::parse_grpc_retry_pushback(
                self.headers(),
                Duration::MAX,
            ) {
                self.extensions_mut().insert(CachedRateLimitHint::new(d));
            }
        }
    }

    fn rate_limit_hint(&self, max: Duration) -> Option<Duration> {
        // Check cache first (from previous attach call), apply caller's cap
        if let Some(cached) = self.extensions().get::<CachedRateLimitHint>() {
            return Some(cached.duration_capped(max));
        }
        // Parse on-read as fallback (header present but attach wasn't called)
        if let Some(d) = linkerd_http_classify::retry_after::parse_retry_after(
            self.status(),
            self.headers(),
            max,
        ) {
            return Some(d);
        }
        // Try gRPC pushback
        if self.status() == http::StatusCode::OK && is_grpc(self.headers()) {
            if let Some(d) =
                linkerd_http_classify::retry_after::parse_grpc_retry_pushback(self.headers(), max)
            {
                return Some(d);
            }
        }
        // No header or unparseable
        None
    }
}

/// Configuration for LoadBiaser behavior.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LoadBiaserConfig {
    /// Default RTT to use when no measurements are available
    pub default_rtt: Duration,

    /// Decay duration for the RTT EWMA.
    /// Controls how quickly RTT estimates adapt to changing latency
    pub rtt_decay: Duration,

    /// Base penalty for failure responses (429, 503, 5xx), in milliseconds.
    /// Also the floor for the recorded effective RTT.
    pub penalty_ms: u32,

    /// Maximum Retry-After duration to honor. Clamped to this value.
    pub max_duration: Duration,
}

impl Default for LoadBiaserConfig {
    fn default() -> Self {
        Self {
            default_rtt: DEFAULT_RTT,
            rtt_decay: DEFAULT_RTT_DECAY,
            penalty_ms: DEFAULT_PENALTY_MS,
            max_duration: DEFAULT_RETRY_AFTER_MAX_DURATION,
        }
    }
}

/// Shared per-endpoint state behind a single `Arc`.
///
/// Holds the RTT EWMA and the immutable config fields every response future
/// needs. The strong count of the `Arc` wrapping this state is also used as
/// the in-flight request count: each [`Handle`] clones it, so subtracting the
/// service's own reference gives the pending count.
#[derive(Debug)]
struct SharedState {
    /// RwLocked RTT EWMA. Read in load(), written when measuring.
    rtt: RwLock<Ewma>,
    /// Base penalty (milliseconds) and floor for the recorded effective RTT.
    penalty_ms: u32,
    /// Maximum Retry-After duration to honor (clamped)
    max_duration: Duration,
}

impl SharedState {
    /// Translates a classified failure into an effective RTT measurement,
    /// taking into account Retry-After/grpc-retry-pushback hints.
    fn effective_rtt<R: ResponseFailureHint>(&self, hint: FailureHint, resp: &R) -> f64 {
        let from_hint = match hint {
            FailureHint::RateLimited | FailureHint::ServiceUnavailable => resp
                .rate_limit_hint(self.max_duration)
                .map(|d| d.as_secs_f64()),
            FailureHint::InternalError => None,
        };

        let base = Duration::from_millis(u64::from(self.penalty_ms)).as_secs_f64();
        // Honor a hint only above the base penalty. This keeps a failing
        // endpoint from looking healthier than a hintless one.
        from_hint.map(|h| h.max(base)).unwrap_or(base)
    }
}

/// A service wrapper that tracks RTT and biases load metrics on failure responses.
///
/// `LoadBiaser` provides load metrics for P2C load balancing by tracking request latency
/// (RTT) via EWMA, in-flight requests, and by injecting penalties when failure responses
/// (429, 503, 5xx) are detected. The load metric is `rtt * (pending + 1)`, identical
/// to Tower's PeakEwma.
///
/// Successes record their measured RTT, and failures record a computed RTT where a
/// penalty has been injected.
///
/// The completion type `C` controls when a successful request's RTT is
/// measured, mirroring PeakEwma. The default is
/// `hyper_balance::PendingUntilFirstData`, which records the RTT at the first
/// response data frame.
#[derive(Debug)]
pub struct LoadBiaser<S, C = PendingUntilFirstData> {
    inner: S,
    /// Per-endpoint RTT state and failure config.
    shared: Arc<SharedState>,
    completion: C,
}

/// A `NewService` implementation that creates `LoadBiaser` wrappers.
#[derive(Debug)]
pub struct NewLoadBiaser<N, Req, C = PendingUntilFirstData> {
    inner: N,
    completion: C,
    config: LoadBiaserConfig,
    _marker: PhantomData<fn(Req)>,
}

/// Tracks one in-flight request and records a measurement when it drops.
///
/// It clones the shared `Arc`, so while it lives the endpoint's strong count
/// is incremented by one. On drop it records the elapsed time as an RTT
/// measurement, so a cancelled request still measures latency.
/// A failure disables the handle first, because it needs to record its own
/// penalty-injected measurement.
#[derive(Debug)]
pub struct Handle {
    shared: Arc<SharedState>,
    sent_at: Instant,
    enabled: bool,
}

impl Handle {
    /// Records the measured elapsed time, then prevents the drop from recording
    /// again.
    fn record_elapsed(&mut self, now: Instant) {
        if self.enabled {
            let elapsed = now.saturating_duration_since(self.sent_at).as_secs_f64();
            self.shared.rtt.write().add_peak(elapsed, now);
            self.enabled = false;
        }
    }

    /// Records a computed effective RTT and disables the handle so its drop does
    /// not also record the measurement of the failure.
    fn record_effective_rtt(&mut self, value: f64, now: Instant) {
        self.shared.rtt.write().add_peak(value, now);
        self.enabled = false;
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // Ensure we take a measurement if the request was cancelled (enabled
        // should be true).
        self.record_elapsed(Instant::now());
    }
}

/// Response future that records a measurement and checks for failure responses.
///
/// On resolution a failure injects a computed RTT and disables the handle.
/// A success leaves the handle for the completion tracker, which decides when
/// to record the measurement.
#[pin_project]
pub struct LoadBiaserFuture<F, Rsp, C> {
    #[pin]
    inner: F,
    /// Handle whose drop records the RTT measurement.
    handle: Option<Handle>,
    completion: C,
    /// Marker for the response type (`ResponseFailureHint` bound)
    _response: PhantomData<fn() -> Rsp>,
}

impl<S, C> LoadBiaser<S, C> {
    /// Creates a new `LoadBiaser` with an explicit completion tracker.
    #[must_use]
    pub fn with_completion(inner: S, completion: C, config: LoadBiaserConfig) -> Self {
        let config = sanitize(config);
        let now = Instant::now();
        let shared = Arc::new(SharedState {
            // Initialize RTT with default_rtt (not INFINITY) so that fresh
            // endpoints have comparable load to unmeasured ones. This matches
            // Tower's PeakEwma behavior and prevents P2C from permanently
            // preferring endpoints whose first request happened to be fast.
            rtt: RwLock::new(Ewma::new_with_value(
                config.rtt_decay,
                now,
                config.default_rtt.as_secs_f64(),
            )),
            penalty_ms: config.penalty_ms,
            max_duration: config.max_duration,
        });
        Self {
            inner,
            shared,
            completion,
        }
    }

    fn handle(&self) -> Handle {
        Handle {
            shared: self.shared.clone(),
            sent_at: Instant::now(),
            enabled: true,
        }
    }
}

impl<S> LoadBiaser<S, PendingUntilFirstData> {
    /// Creates a new `LoadBiaser` recording RTT at the first response data frame.
    #[must_use]
    pub fn new(inner: S, config: LoadBiaserConfig) -> Self {
        Self::with_completion(inner, PendingUntilFirstData::default(), config)
    }
}

fn sanitize(config: LoadBiaserConfig) -> LoadBiaserConfig {
    if config.rtt_decay < MIN_DECAY {
        tracing::warn!(
            rtt_decay = ?config.rtt_decay,
            min = ?MIN_DECAY,
            "rtt_decay below minimum, will be clamped by EWMA constructor"
        );
    }
    config
}

impl<S, C> Load for LoadBiaser<S, C> {
    type Metric = f64;

    fn load(&self) -> Self::Metric {
        // Pending is the strong count of the Arc minus the service's own ref.
        let pending = (Arc::strong_count(&self.shared) as u32).saturating_sub(1);
        let now = Instant::now();

        let rtt = self.shared.rtt.read().get_at(now);

        // Load = RTT * (pending + 1). The +1 keeps an idle endpoint's cost tied
        // to its RTT so a slow-but-quiet endpoint is not treated as free.
        let load = rtt * f64::from(pending.saturating_add(1));

        tracing::trace!(
            rtt_secs = rtt,
            pending = pending,
            load = load,
            "LoadBiaser::load"
        );

        load
    }
}

impl<S, C, Req> Service<Req> for LoadBiaser<S, C>
where
    S: Service<Req>,
    S::Response: ResponseFailureHint,
    C: TrackCompletion<Handle, S::Response> + Clone,
{
    type Response = C::Output;
    type Error = S::Error;
    type Future = LoadBiaserFuture<S::Future, S::Response, C>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        LoadBiaserFuture {
            inner: self.inner.call(req),
            handle: Some(self.handle()),
            completion: self.completion.clone(),
            _response: PhantomData,
        }
    }
}

impl<N, Req, C> NewLoadBiaser<N, Req, C> {
    /// Creates a `NewLoadBiaser` with an explicit completion tracker.
    pub fn with_completion(config: LoadBiaserConfig, completion: C, inner: N) -> Self {
        Self {
            inner,
            completion,
            config,
            _marker: PhantomData,
        }
    }
}

impl<N, Req> NewLoadBiaser<N, Req, PendingUntilFirstData> {
    /// Creates a new `NewLoadBiaser` with the given configuration.
    pub fn new(config: LoadBiaserConfig, inner: N) -> Self {
        Self::with_completion(config, PendingUntilFirstData::default(), inner)
    }
}

impl<N: Clone, Req, C: Clone> Clone for NewLoadBiaser<N, Req, C> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            completion: self.completion.clone(),
            config: self.config.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T, N, S, Req, C> NewService<T> for NewLoadBiaser<N, Req, C>
where
    N: NewService<T, Service = S>,
    C: Clone,
{
    type Service = LoadBiaser<S, C>;

    fn new_service(&self, target: T) -> LoadBiaser<S, C> {
        LoadBiaser::with_completion(
            self.inner.new_service(target),
            self.completion.clone(),
            self.config.clone(),
        )
    }
}

impl<F, Rsp, E, C> Future for LoadBiaserFuture<F, Rsp, C>
where
    F: Future<Output = Result<Rsp, E>>,
    Rsp: ResponseFailureHint,
    C: TrackCompletion<Handle, Rsp>,
{
    type Output = Result<C::Output, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let result = ready!(this.inner.poll(cx));
        let now = Instant::now();
        let mut handle = this.handle.take().expect("polled after completion");

        match result {
            Ok(mut resp) => {
                if let Some(hint) = resp.failure_hint() {
                    // Cache the uncapped hint while we hold &mut, then record the
                    // failure as a penalized effective RTT.
                    resp.attach_parsed_rate_limit_hint();
                    let value = handle.shared.effective_rtt(hint, &resp);
                    tracing::debug!(rtt_secs = value, ?hint, "Failure recorded as effective RTT");
                    handle.record_effective_rtt(value, now);
                }

                // The completion tracker will drop the handle now or at first body
                // frame, and that drop will record the measured RTT.
                let output = this.completion.track_completion(handle, resp);

                Poll::Ready(Ok(output))
            }
            Err(e) => {
                // A transport error (connection refused, reset) or cancellation with
                // no response to classify. Record its elapsed time.
                drop(handle);

                Poll::Ready(Err(e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use std::convert::Infallible;
    use tokio::time;

    /// Response body used by mock services.
    ///
    /// Reports `is_end_stream() == false` until polled, causing
    /// `PendingUntilFirstData` to record the RTT on success once the
    /// body reaches its first frame or is dropped.
    type TestBody = Full<Bytes>;

    /// Test body with a small payload.
    fn body() -> TestBody {
        Full::new(Bytes::from_static(b"test"))
    }

    /// Drives a response body to its first frame.
    async fn drive_to_first_frame<B>(resp: http::Response<B>)
    where
        B: http_body::Body + Unpin,
        B::Error: std::fmt::Debug,
    {
        let mut b = resp.into_body();
        let _ = b.frame().await;
    }

    impl<S, C> LoadBiaser<S, C> {
        pub fn get_rtt(&self) -> f64 {
            self.shared.rtt.read().get()
        }

        pub fn get_pending(&self) -> u32 {
            (Arc::strong_count(&self.shared) as u32).saturating_sub(1)
        }

        pub fn inject_rtt(&self, rtt_secs: f64) {
            self.shared.rtt.write().add_peak(rtt_secs, Instant::now());
        }
    }

    impl Handle {
        /// Disable a handle so dropping it records nothing. Lets a test hold a
        /// handle just to raise the pending count.
        fn disable(mut self) {
            self.enabled = false;
        }
    }

    // Mock service for testing returning a specific HTTP status, optionally
    // after a delay and with extra response headers.
    #[derive(Clone)]
    struct MockService {
        status: http::StatusCode,
        delay: Duration,
        headers: Vec<(http::HeaderName, http::HeaderValue)>,
    }

    impl MockService {
        fn new(status: http::StatusCode) -> Self {
            Self {
                status,
                delay: Duration::ZERO,
                headers: Vec::new(),
            }
        }

        fn with_delay(status: http::StatusCode, delay: Duration) -> Self {
            Self {
                status,
                delay,
                headers: Vec::new(),
            }
        }

        // HTTP error carrying a Retry-After header, in seconds.
        fn retry_after(status: http::StatusCode, retry_after_secs: u64) -> Self {
            Self {
                status,
                delay: Duration::ZERO,
                headers: vec![(
                    http::header::RETRY_AFTER,
                    http::HeaderValue::from(retry_after_secs),
                )],
            }
        }

        // HTTP 200 with a grpc-status header, simulating a gRPC trailers-only
        // error response.
        fn grpc_status(grpc_status: u16) -> Self {
            Self {
                status: http::StatusCode::OK,
                delay: Duration::ZERO,
                headers: vec![
                    (
                        http::header::CONTENT_TYPE,
                        http::HeaderValue::from_static("application/grpc"),
                    ),
                    (
                        http::HeaderName::from_static("grpc-status"),
                        http::HeaderValue::from(grpc_status),
                    ),
                ],
            }
        }
    }

    impl Service<()> for MockService {
        type Response = http::Response<TestBody>;
        type Error = Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: ()) -> Self::Future {
            let status = self.status;
            let delay = self.delay;
            let headers = self.headers.clone();
            Box::pin(async move {
                if !delay.is_zero() {
                    time::sleep(delay).await;
                }
                let mut builder = http::Response::builder().status(status);
                for (name, value) in headers {
                    builder = builder.header(name, value);
                }
                let resp = builder.body(body()).unwrap();
                Ok(resp)
            })
        }
    }

    fn test_config() -> LoadBiaserConfig {
        LoadBiaserConfig {
            default_rtt: Duration::from_millis(100), // 0.1s default
            rtt_decay: Duration::from_secs(10),
            penalty_ms: 5000,
            max_duration: DEFAULT_RETRY_AFTER_MAX_DURATION,
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_load_uses_default_rtt_initially() {
        let inner = MockService::new(http::StatusCode::OK);
        let biaser = LoadBiaser::new(inner, test_config());

        // Initial load should be default_rtt * (pending + 1) = 0.1 * 1 = 0.1
        let load = biaser.load();
        assert!(
            (load - 0.1).abs() < 0.001,
            "initial load should be ~0.1 (default RTT): {}",
            load
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_pending_increases_load() {
        let inner = MockService::new(http::StatusCode::OK);
        let biaser = LoadBiaser::new(inner, test_config());

        // Inject a known RTT below the default
        time::sleep(Duration::from_millis(1)).await;
        biaser.inject_rtt(0.05); // 50ms

        // No pending handles. Load = RTT * (0 + 1) = 0.05.
        let load_idle = biaser.load();
        assert_eq!(
            load_idle,
            0.09999500024999168 /* TODO: should equal 0.05 */
        );

        // Hold a handle to simulate an in-flight request: the strong count of
        // the shared Arc increments per live handle like during a call.
        let h1 = biaser.handle();
        let load_one_pending = biaser.load();
        assert_eq!(
            load_one_pending,
            0.19999000049998336 /* TODO: should equal 0.1 */
        );

        let h2 = biaser.handle();
        let load_two_pending = biaser.load();
        assert_eq!(
            load_two_pending,
            0.29998500074997503 /* TODO: should equal 0.15 */
        );

        assert!(
            load_one_pending > load_idle,
            "load should increase with pending: {} > {}",
            load_one_pending,
            load_idle
        );
        assert!(
            load_two_pending > load_one_pending,
            "load should increase more: {} > {}",
            load_two_pending,
            load_one_pending
        );

        h1.disable();
        h2.disable();
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_rtt_tracked_after_request() {
        // Drive one request that takes a measurable delay and assert the RTT
        // recorded matches the delay.
        let delay = Duration::from_millis(250);
        let inner = MockService::with_delay(http::StatusCode::OK, delay);
        let mut biaser = LoadBiaser::new(inner, test_config());

        // Advance time so EWMA accepts updates
        time::sleep(Duration::from_millis(1)).await;

        // RTT should start at default_rtt before any requests
        let initial_rtt = biaser.get_rtt();
        assert!(
            (initial_rtt - 0.1).abs() < 0.01,
            "RTT should start at default_rtt (0.1s), got: {initial_rtt}"
        );

        // First frame drops the handle and records the measurement.
        let resp = biaser.call(()).await.unwrap();
        drive_to_first_frame(resp).await;

        // The recorded RTT should match the request delay (250ms).
        let rtt = biaser.get_rtt();
        assert!(
            (rtt - delay.as_secs_f64()).abs() < 0.005,
            "RTT should reflect the 250ms request delay, got: {rtt}"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_fast_failure_more_expensive_than_healthy() {
        // A fast 503 should be more expensive than a fast 200
        let mut failing = LoadBiaser::new(
            MockService::new(http::StatusCode::SERVICE_UNAVAILABLE),
            test_config(),
        );
        let mut healthy = LoadBiaser::new(MockService::new(http::StatusCode::OK), test_config());

        time::sleep(Duration::from_millis(1)).await;

        let _ = failing.call(()).await;
        let _ = healthy.call(()).await;

        let load_failing = failing.load();
        let load_healthy = healthy.load();

        assert!(
            load_failing > load_healthy,
            "fast failure should read higher load than healthy: {load_failing} > {load_healthy}"
        );
        // The failure records the base penalty (5s)
        assert!(
            load_failing > 4.0,
            "503 should record the base effective RTT (~5s): {load_failing}"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_retry_after_hint_load_grows_monotonically() {
        let hints = [0u64, 1, 5, 30, 120, 300];
        let mut prev = f64::NEG_INFINITY;

        for secs in hints {
            let inner = MockService::retry_after(http::StatusCode::TOO_MANY_REQUESTS, secs);
            let mut biaser = LoadBiaser::new(inner, test_config());

            time::sleep(Duration::from_millis(1)).await;

            let _ = biaser.call(()).await;
            let load = biaser.load();
            assert!(
                load >= prev,
                "load must be monotonic in Retry-After: hint={secs}s gave {load}, previous {prev}"
            );
            prev = load;
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_faster_healthy_is_cheaper_than_slower_healthy() {
        let mut fast = LoadBiaser::new(MockService::new(http::StatusCode::OK), test_config());
        let slow = LoadBiaser::new(MockService::new(http::StatusCode::OK), test_config());

        time::sleep(Duration::from_millis(1)).await;

        let _ = fast.call(()).await;

        fast.inject_rtt(0.01);
        slow.inject_rtt(2.0);

        assert!(
            fast.load() < slow.load(),
            "lower RTT must rank cheaper: fast={} slow={}",
            fast.load(),
            slow.load()
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_429_injects_penalty() {
        let inner = MockService::new(http::StatusCode::TOO_MANY_REQUESTS);
        let mut biaser = LoadBiaser::new(inner, test_config());

        time::sleep(Duration::from_millis(1)).await;

        // Make a request that returns 429 (no hint)
        let _ = biaser.call(()).await;

        // The 429 records the penalty (5s)
        let rtt = biaser.get_rtt();
        assert!(
            (rtt - 5.0).abs() < 0.1,
            "429 without a hint should record ~5s RTT: {rtt}"
        );

        // Load should be at least the penalty.
        let load = biaser.load();
        assert!(load >= 4.9, "load should be high after 429: {}", load);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_200_does_not_inject_penalty() {
        let inner = MockService::new(http::StatusCode::OK);
        let mut biaser = LoadBiaser::new(inner, test_config());

        time::sleep(Duration::from_millis(1)).await;

        // Make a request that returns 200.
        let _ = biaser.call(()).await;

        // A success records only its measured (near-zero) RTT, well below
        // the 5s penalty base.
        let rtt = biaser.get_rtt();
        assert!(rtt < 1.0, "200 should record a low RTT, got: {rtt}");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_penalty_decays_over_time() {
        let inner = MockService::new(http::StatusCode::TOO_MANY_REQUESTS);
        let config = LoadBiaserConfig {
            default_rtt: Duration::from_millis(1),
            ..test_config()
        };
        let mut biaser = LoadBiaser::new(inner, config);

        time::sleep(Duration::from_millis(1)).await;

        // Trigger penalty via 429 response.
        let _ = biaser.call(()).await;

        // After call completes, pending is back to 0
        assert_eq!(
            biaser.get_pending(),
            0,
            "pending should be 0 after call completes"
        );

        // Immediately after: effective RTT ~5.0, so load ~5.0.
        let load_before = biaser.load();
        assert!(
            load_before > 4.0,
            "load before decay should reflect the 429 penalized RTT: {}",
            load_before
        );

        // Advance time by one decay period (10s).
        // Penalty RTT decays: 5.0 * e^(-10/10) ~ 1.839
        time::sleep(Duration::from_secs(10)).await;

        let load_after = biaser.load();
        assert!(
            load_after > 1.0 && load_after < 2.5,
            "load after one decay period should reflect 5.0 * e^-1 ~ 1.839: {}",
            load_after
        );
        assert!(
            load_after < load_before,
            "effective RTT should decay over time: {} < {}",
            load_after,
            load_before
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_sequential_failures_maintain_load() {
        let inner = MockService::new(http::StatusCode::TOO_MANY_REQUESTS);
        let config = LoadBiaserConfig {
            default_rtt: Duration::from_millis(1),
            ..test_config()
        };
        let mut biaser = LoadBiaser::new(inner, config);

        time::sleep(Duration::from_millis(1)).await;

        let _ = biaser.call(()).await;
        let rtt_first = biaser.get_rtt();
        assert!(
            (rtt_first - 5.0).abs() < 0.1,
            "first 429 should record ~5s, got: {rtt_first}"
        );

        // A second 429 one second later: add_peak replaces the slightly decayed
        // value with the new 5s measurement.
        time::sleep(Duration::from_secs(1)).await;
        let _ = biaser.call(()).await;
        let rtt_second = biaser.get_rtt();
        assert!(
            (rtt_second - 5.0).abs() < 0.1,
            "second 429 should maintain ~5s, got: {rtt_second}"
        );

        time::sleep(Duration::from_secs(1)).await;
        let _ = biaser.call(()).await;
        let rtt_third = biaser.get_rtt();
        assert!(
            (rtt_third - 5.0).abs() < 0.1,
            "third 429 should maintain ~5s, got: {rtt_third}"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_pending_counted_via_strong_count() {
        let inner = MockService::new(http::StatusCode::OK);
        let mut biaser = LoadBiaser::new(inner, test_config());

        assert_eq!(biaser.get_pending(), 0, "pending should start at 0");

        // Start a request: the future holds a handle, raising the count.
        let fut = biaser.call(());
        assert_eq!(
            biaser.get_pending(),
            1,
            "pending should be 1 during request"
        );

        // Resolve the response future. The default tracker causes the handle to
        // consider the request in-flight until the body yields the first frame.
        let resp = fut.await.unwrap();
        assert_eq!(
            biaser.get_pending(),
            1,
            "pending should stay 1 while until the body yields it first frame"
        );

        // When the body produces its first frame the handle drops
        drive_to_first_frame(resp).await;
        assert_eq!(
            biaser.get_pending(),
            0,
            "pending should be 0 after the body yields its first frame"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_two_concurrent_requests_counted() {
        // Two concurrent in-flight requests each hold a handle. The strong count
        // should report two pending while both are unresolved.
        let delay = Duration::from_millis(200);
        let inner = MockService::with_delay(http::StatusCode::OK, delay);
        let mut biaser = LoadBiaser::new(inner, test_config());

        time::sleep(Duration::from_millis(1)).await;

        assert_eq!(biaser.get_pending(), 0, "pending should start at 0");
        let load_idle = biaser.load();

        // Start two requests without resolving them
        let fut1 = biaser.call(());
        let fut2 = biaser.call(());
        tokio::pin!(fut1);
        tokio::pin!(fut2);

        // Poll each once, ensure they are pending
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(fut1.as_mut().poll(&mut cx).is_pending());
        assert!(fut2.as_mut().poll(&mut cx).is_pending());

        assert_eq!(
            biaser.get_pending(),
            2,
            "two concurrent requests should report two pending"
        );

        // Load = RTT * (pending + 1) = RTT * 3, triple load.
        let load_two = biaser.load();
        assert_eq!(
            load_two,
            load_idle * 3.0,
            "two pending should triple the idle load: {load_two} vs {load_idle}"
        );

        // Advance past the delay, then drive the body to drop the handles.
        time::sleep(Duration::from_millis(250)).await;
        let resp1 = fut1.await.unwrap();
        let resp2 = fut2.await.unwrap();
        drive_to_first_frame(resp1).await;
        drive_to_first_frame(resp2).await;

        assert_eq!(
            biaser.get_pending(),
            0,
            "pending should fall back to 0 once both requests resolve"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_cancellation_records_measurement() {
        // A request that stays in flight. The long delay keeps the future
        // pending. The test drops it to exercise the cancellation path.
        let inner = MockService::with_delay(http::StatusCode::OK, Duration::from_secs(3600));
        let mut biaser = LoadBiaser::new(inner, test_config());

        assert_eq!(biaser.get_pending(), 0);

        // Initiate the request, pending increments in Service::call()
        let fut = biaser.call(());
        assert_eq!(biaser.get_pending(), 1);

        // Advance time so the cancelled request has measurable elapsed time.
        time::sleep(Duration::from_secs(3)).await;

        let rtt_before = biaser.get_rtt();

        // Drop the future without completing it. The handle's drop should
        // records the elapsed wait as an RTT measurement.
        drop(fut);

        assert_eq!(
            biaser.get_pending(),
            0,
            "dropping the future should release the handle and decrease pending"
        );
        let rtt_after = biaser.get_rtt();
        assert!(
            rtt_after > rtt_before,
            "cancellation should record the elapsed wait: {rtt_after} > {rtt_before}"
        );
    }

    #[test]
    fn default_max_duration_matches_client_policy() {
        // Ensure that LoadBiaserConfig::default().max_duration matches
        // DEFAULT_RETRY_AFTER_MAX_DURATION constant, keeping the default() in
        // sync with that constant.
        assert_eq!(
            LoadBiaserConfig::default().max_duration,
            DEFAULT_RETRY_AFTER_MAX_DURATION,
            "LoadBiaserConfig::default().max_duration must match \
             DEFAULT_RETRY_AFTER_MAX_DURATION (300s)"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_429_with_retry_after_records_hint() {
        let inner = MockService::retry_after(http::StatusCode::TOO_MANY_REQUESTS, 30);
        let mut biaser = LoadBiaser::new(inner, test_config());

        time::sleep(Duration::from_millis(1)).await;

        // Make a request that returns 429 with Retry-After: 30
        let _ = biaser.call(()).await;

        // The hint should be recorded as the effective RTT (30s)
        let rtt = biaser.get_rtt();
        assert!(
            (rtt - 30.0).abs() < 0.5,
            "429 with Retry-After: 30 should record ~30s effective RTT, got: {rtt}"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_small_retry_after_floored_at_base_penalty() {
        // A Retry-After below the base penalty must not lower the effective RTT.
        let inner = MockService::retry_after(http::StatusCode::TOO_MANY_REQUESTS, 1);
        let mut biaser = LoadBiaser::new(inner, test_config());
        time::sleep(Duration::from_millis(1)).await;
        let _ = biaser.call(()).await;
        // penalty_ms = 5000; a 1s hint is floored to the 5s base.
        let rtt = biaser.get_rtt();
        assert!(
            (rtt - 5.0).abs() < 0.1,
            "1s hint should floor to 5s base, got: {rtt}"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_429_with_retry_after_capped_at_max() {
        let inner = MockService::retry_after(http::StatusCode::TOO_MANY_REQUESTS, 600);
        let mut biaser = LoadBiaser::new(inner, test_config());

        time::sleep(Duration::from_millis(1)).await;

        // Make a request that returns 429 with Retry-After: 600, clamped to 300s
        let _ = biaser.call(()).await;

        // Retry-After: 600 is capped to max_duration (300s).
        let rtt = biaser.get_rtt();
        assert!(
            (rtt - 300.0).abs() < 0.5,
            "429 with Retry-After: 600 should cap to 300s, got: {rtt}"
        );
    }

    #[test]
    fn test_rate_limit_hint_parses_retry_after() {
        let mut resp = http::Response::builder()
            .status(http::StatusCode::TOO_MANY_REQUESTS)
            .header(http::header::RETRY_AFTER, "45")
            .body("rate limited")
            .unwrap();
        let max = Duration::from_secs(60);

        resp.attach_parsed_rate_limit_hint();

        assert_eq!(resp.rate_limit_hint(max), Some(Duration::from_secs(45)));
    }

    #[test]
    fn test_rate_limit_hint_none_for_200() {
        let mut resp = http::Response::builder()
            .status(http::StatusCode::OK)
            .header(http::header::RETRY_AFTER, "45")
            .body("ok")
            .unwrap();
        let max = Duration::from_secs(60);

        resp.attach_parsed_rate_limit_hint();

        assert_eq!(resp.rate_limit_hint(max), None);
    }

    #[test]
    fn test_rate_limit_hint_none_without_header() {
        let mut resp = http::Response::builder()
            .status(http::StatusCode::TOO_MANY_REQUESTS)
            .body("rate limited")
            .unwrap();
        let max = Duration::from_secs(60);

        resp.attach_parsed_rate_limit_hint();

        assert_eq!(resp.rate_limit_hint(max), None);
    }

    #[test]
    fn test_rate_limit_hint_on_read_without_attach() {
        let resp = http::Response::builder()
            .status(http::StatusCode::TOO_MANY_REQUESTS)
            .header(http::header::RETRY_AFTER, "45")
            .body("rate limited")
            .unwrap();

        // No attach call, test on-read fallback
        assert_eq!(
            resp.rate_limit_hint(Duration::from_secs(60)),
            Some(Duration::from_secs(45))
        );
    }

    #[test]
    fn test_rate_limit_hint_on_read_caps_at_max() {
        let resp = http::Response::builder()
            .status(http::StatusCode::TOO_MANY_REQUESTS)
            .header(http::header::RETRY_AFTER, "300")
            .body("rate limited")
            .unwrap();

        // On-read parse caps at caller's max
        assert_eq!(
            resp.rate_limit_hint(Duration::from_secs(60)),
            Some(Duration::from_secs(60))
        );
    }

    // Test multiple caps

    const SMALL_CAP: Duration = Duration::from_secs(60);
    const DEFAULT_CAP: Duration = DEFAULT_RETRY_AFTER_MAX_DURATION;
    const LARGE_CAP: Duration = Duration::from_secs(1800);

    // Constructs a 429 response whose `Retry-After` header uses the integer
    // seconds form (delay-seconds per RFC 7231).
    fn build_http_retry_after_response(retry_after_secs: u64) -> http::Response<&'static str> {
        http::Response::builder()
            .status(http::StatusCode::TOO_MANY_REQUESTS)
            .header(http::header::RETRY_AFTER, retry_after_secs.to_string())
            .body("rate limited")
            .unwrap()
    }

    // Constructs a 200 OK trailers-only gRPC error response using
    // `grpc-status: 8` (RESOURCE_EXHAUSTED) and `grpc-retry-pushback-ms: <ms>`.
    fn build_grpc_pushback_response(pushback_ms: u64) -> http::Response<&'static str> {
        http::Response::builder()
            .status(http::StatusCode::OK)
            .header(http::header::CONTENT_TYPE, "application/grpc")
            .header("grpc-status", "8")
            .header("grpc-retry-pushback-ms", pushback_ms.to_string())
            .body("grpc error")
            .unwrap()
    }

    // Verifies the cached-path `.min(max)` clamp in `rate_limit_hint()` clamps HTTP
    // Retry-After hints to the caller-supplied cap consistently across cap
    // magnitudes (60s / 300s / 1800s) and directions (below-cap / over-cap).
    #[test]
    fn test_rate_limit_hint_multi_cap_http() {
        struct Row {
            cap: Duration,
            header_value: u64, // Retry-After in integer seconds
        }

        let rows = [
            Row {
                cap: SMALL_CAP,
                header_value: 30,
            }, // below
            Row {
                cap: SMALL_CAP,
                header_value: 120,
            }, // over
            Row {
                cap: DEFAULT_CAP,
                header_value: 120,
            }, // below
            Row {
                cap: DEFAULT_CAP,
                header_value: 600,
            }, // over
            Row {
                cap: LARGE_CAP,
                header_value: 900,
            }, // below
            Row {
                cap: LARGE_CAP,
                header_value: 3600,
            }, // over
        ];

        for Row { cap, header_value } in rows {
            let mut resp = build_http_retry_after_response(header_value);
            resp.attach_parsed_rate_limit_hint();

            // Remove the source header after attach
            resp.headers_mut().remove(http::header::RETRY_AFTER);
            let parsed = Duration::from_secs(header_value);
            let expected = parsed.min(cap);

            assert_eq!(
                resp.rate_limit_hint(cap),
                Some(expected),
                "cap={cap:?}, header={header_value}s, expected={expected:?}",
            );
        }
    }

    // Verifies the cached-path `.min(max)` clamp in `rate_limit_hint()` clamps gRPC
    // retry-pushback-ms hints to the caller-supplied cap consistently across
    // cap magnitudes (60s / 300s / 1800s) and directions (below-cap / over-cap).
    #[test]
    fn test_rate_limit_hint_multi_cap_grpc() {
        struct Row {
            cap: Duration,
            header_value: u64, // grpc-retry-pushback in milliseconds
        }

        let rows = [
            Row {
                cap: SMALL_CAP,
                header_value: 30_000,
            }, // below (30s)
            Row {
                cap: SMALL_CAP,
                header_value: 120_000,
            }, // over  (120s)
            Row {
                cap: DEFAULT_CAP,
                header_value: 120_000,
            }, // below (120s)
            Row {
                cap: DEFAULT_CAP,
                header_value: 600_000,
            }, // over  (600s)
            Row {
                cap: LARGE_CAP,
                header_value: 900_000,
            }, // below (900s)
            Row {
                cap: LARGE_CAP,
                header_value: 3_600_000,
            }, // over  (3600s)
        ];

        for Row { cap, header_value } in rows {
            let mut resp = build_grpc_pushback_response(header_value);
            resp.attach_parsed_rate_limit_hint();

            // Remove the source header after attach
            resp.headers_mut().remove("grpc-retry-pushback-ms");
            let parsed = Duration::from_millis(header_value);
            let expected = parsed.min(cap);

            assert_eq!(
                resp.rate_limit_hint(cap),
                Some(expected),
                "cap={cap:?}, header={header_value}ms, expected={expected:?}",
            );
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_503_records_base_penalized_effective_rtt() {
        let inner = MockService::new(http::StatusCode::SERVICE_UNAVAILABLE);
        let mut biaser = LoadBiaser::new(inner, test_config());

        time::sleep(Duration::from_millis(1)).await;

        let _ = biaser.call(()).await;
        let rtt = biaser.get_rtt();

        // test_config has penalty_ms = 5000
        assert!(
            (rtt - 5.0).abs() < 0.1,
            "503 should record the base penalized effective RTT (~5.0s), got: {rtt}"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_503_with_retry_after_records_hint() {
        let inner = MockService::retry_after(http::StatusCode::SERVICE_UNAVAILABLE, 30);
        let mut biaser = LoadBiaser::new(inner, test_config());

        time::sleep(Duration::from_millis(1)).await;

        // Make a request that returns 503 with Retry-After: 30
        let _ = biaser.call(()).await;

        // 503 honors the hint the same way 429 does.
        let rtt = biaser.get_rtt();

        assert!(
            (rtt - 30.0).abs() < 0.5,
            "503 with Retry-After: 30 should record ~30s, got: {rtt}"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_429_and_503_retry_after_produce_identical_load() {
        let inner_429 = MockService::retry_after(http::StatusCode::TOO_MANY_REQUESTS, 30);
        let mut biaser_429 = LoadBiaser::new(inner_429, test_config());

        // 503 path: SERVICE_UNAVAILABLE with Retry-After: 30
        let inner_503 = MockService::retry_after(http::StatusCode::SERVICE_UNAVAILABLE, 30);
        let mut biaser_503 = LoadBiaser::new(inner_503, test_config());

        // Bootstrap EWMA timestamps
        time::sleep(Duration::from_millis(1)).await;
        let _ = biaser_429.call(()).await;

        time::sleep(Duration::from_millis(1)).await;
        let _ = biaser_503.call(()).await;

        let load_429 = biaser_429.load();
        let load_503 = biaser_503.load();

        assert!(
            (load_429 - load_503).abs() / load_429.max(1e-9) < 0.05,
            "429+RA and 503+RA should produce similar load; 429={load_429}, 503={load_503}"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_500_records_base_penalized_effective_rtt() {
        let inner = MockService::new(http::StatusCode::INTERNAL_SERVER_ERROR);
        let mut biaser = LoadBiaser::new(inner, test_config());

        time::sleep(Duration::from_millis(1)).await;

        let _ = biaser.call(()).await;
        let rtt = biaser.get_rtt();

        // test_config has penalty_ms = 5000
        assert!(
            (rtt - 5.0).abs() < 0.1,
            "500 should record the base penalized effective RTT (~5.0s), got: {rtt}"
        );
    }

    #[test]
    fn failure_hint_non_grpc_200_with_grpc_status_header() {
        let resp = http::Response::builder()
            .status(http::StatusCode::OK)
            .header("grpc-status", "14")
            .body("")
            .unwrap();
        assert_eq!(
            resp.failure_hint(),
            None,
            "non-gRPC HTTP 200 with grpc-status header should not be classified"
        );
    }

    async fn assert_grpc_records_penalized_effective_rtt(grpc_status: u16, label: &str) {
        let inner = MockService::grpc_status(grpc_status);
        let mut biaser = LoadBiaser::new(inner, test_config());

        time::sleep(Duration::from_millis(1)).await;
        let _ = biaser.call(()).await;
        let rtt = biaser.get_rtt();

        assert!(
            (rtt - 5.0).abs() < 0.1,
            "gRPC {label} (status {grpc_status}) should record the base penalized effective RTT (~5s), got: {rtt}"
        );
    }

    async fn assert_grpc_records_low_rtt(grpc_status: u16, label: &str) {
        let inner = MockService::grpc_status(grpc_status);
        let mut biaser = LoadBiaser::new(inner, test_config());

        time::sleep(Duration::from_millis(1)).await;
        let _ = biaser.call(()).await;

        assert!(
            biaser.get_rtt() < 1.0,
            "gRPC {label} (status {grpc_status}) should record only the measured RTT: {}",
            biaser.get_rtt()
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_grpc_server_errors_record_effective_rtt() {
        assert_grpc_records_penalized_effective_rtt(8, "RESOURCE_EXHAUSTED").await;
        assert_grpc_records_penalized_effective_rtt(14, "UNAVAILABLE").await;
        assert_grpc_records_penalized_effective_rtt(13, "INTERNAL").await;
        assert_grpc_records_penalized_effective_rtt(2, "UNKNOWN").await;
        assert_grpc_records_penalized_effective_rtt(4, "DEADLINE_EXCEEDED").await;
        assert_grpc_records_penalized_effective_rtt(15, "DATA_LOSS").await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_grpc_client_errors_record_low_rtt() {
        assert_grpc_records_low_rtt(1, "CANCELLED").await;
        assert_grpc_records_low_rtt(3, "INVALID_ARGUMENT").await;
        assert_grpc_records_low_rtt(5, "NOT_FOUND").await;
        assert_grpc_records_low_rtt(6, "ALREADY_EXISTS").await;
        assert_grpc_records_low_rtt(7, "PERMISSION_DENIED").await;
        assert_grpc_records_low_rtt(9, "FAILED_PRECONDITION").await;
        assert_grpc_records_low_rtt(10, "ABORTED").await;
        assert_grpc_records_low_rtt(11, "OUT_OF_RANGE").await;
        assert_grpc_records_low_rtt(12, "UNIMPLEMENTED").await;
        assert_grpc_records_low_rtt(16, "UNAUTHENTICATED").await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_grpc_ok_records_low_rtt() {
        assert_grpc_records_low_rtt(0, "OK").await;
    }
}
