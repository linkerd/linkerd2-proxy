//! Load tracking with response failure awareness.
//!
//! This module provides a `LoadBiaser` wrapper that tracks request latency (RTT)
//! and detects failure responses (HTTP 429, 503, 5xx), applying artificial
//! penalties. Unlike Tower's `PeakEwma`, this implementation uses
//! `linkerd_ewma::Ewma` and returns `f64` metrics directly, enabling
//! integration with P2C load balancing.
//!
//! This can wrap any service (`Load` trait not required) and it tracks RTT via
//! EWMA, which is updated when responses complete, and pending requests for load
//! calculation.
//!
//! When a failure response is detected, a penalty is applied and the EWMA jumps
//! to a high value. The load is calculated as `max(rtt * (pending + 1), penalty)`
//! (so at least RTT with no pending requests).
//!
//! Both RTT and penalty decay over time via the EWMA.
//!
//! For this type to work responses must implement the `ResponseFailureHint`
//! trait, which classifies responses into failure categories (rate-limited,
//! service unavailable, internal error). For non-HTTP responses the default
//! implementation returns no failure hint, so only RTT tracking occurs.
//!
//! For gRPC, `failure_hint()` detects `RESOURCE_EXHAUSTED` (code 8) and
//! `UNAVAILABLE` (code 14) from the `grpc-status` header in trailers-only
//! (unary) responses. Streaming gRPC puts `grpc-status` in trailers which
//! are not visible at response-head time; the circuit breaker handles
//! streaming gRPC failures via `GrpcRetryPushbackClassifyEos`.

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use futures::ready;
use linkerd_ewma::{Ewma, MIN_DECAY};
use linkerd_stack::NewService;
use parking_lot::RwLock;
use pin_project::pin_project;
use std::{
    future::Future,
    marker::PhantomData,
    pin::Pin,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::time::Instant;
use tower::load::Load;
use tower_service::Service;

/// Default maximum duration for Retry-After hints.
pub const DEFAULT_RETRY_AFTER_MAX_DURATION: Duration = Duration::from_secs(300);

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
/// penalties. Default implementations return None (no failure detected),
/// which is appropriate for non-HTTP transports.
///
/// The trait splits rate limit hint access into two methods to avoid
/// requiring `&mut self` on the read path:
/// - `attach_parsed_rate_limit_hint(&mut self)`: parse and cache (needs `&mut`)
/// - `rate_limit_hint(&self, max)`: read cached value or parse on-read (only needs `&self`)
///
/// # Stack ordering and caching
///
/// In the current proxy stack `RetryAfterClassify::start()` (the circuit breaker's
/// classifier) calls `rate_limit_hint()` before `LoadBiaserFuture::poll()` calls
/// `attach_parsed_rate_limit_hint()`, because responses flow inner-to-outer. This
/// means the cache is always cold for the circuit breaker path.
pub trait ResponseFailureHint {
    /// Returns a failure hint if the response indicates a failure condition.
    fn failure_hint(&self) -> Option<FailureHint> {
        None
    }

    /// Parse and cache the raw (uncapped) rate limit hint from this response.
    ///
    /// The raw uncapped value is cached so that each consumer can apply their
    /// own cap via `rate_limit_hint(max)`.
    fn attach_parsed_rate_limit_hint(&mut self) {}

    /// Returns the rate limit hint if available.
    ///
    /// Checks cached value first (from a previous `attach_parsed_rate_limit_hint` call).
    /// If no cached value, attempts to parse the header directly (capping at `max`).
    /// Returns `None` only if the header is absent or unparseable.
    fn rate_limit_hint(&self, _max: Duration) -> Option<Duration> {
        None
    }
}

/// HTTP responses classify failures by status code and parse Retry-After hints.
///
/// For gRPC responses (which arrive as HTTP 200 with `grpc-status` in headers
/// for trailers-only/unary errors), the `grpc-status` header is checked when
/// the HTTP status is 200:
/// - gRPC status 8 (RESOURCE_EXHAUSTED) -> `RateLimited`
/// - gRPC status 14 (UNAVAILABLE) -> `ServiceUnavailable`
/// - Other non-zero gRPC status codes -> `InternalError`
impl<B> ResponseFailureHint for http::Response<B> {
    fn failure_hint(&self) -> Option<FailureHint> {
        let status = self.status();
        if status == http::StatusCode::TOO_MANY_REQUESTS {
            Some(FailureHint::RateLimited)
        } else if status == http::StatusCode::SERVICE_UNAVAILABLE {
            Some(FailureHint::ServiceUnavailable)
        } else if status.is_server_error() {
            Some(FailureHint::InternalError)
        } else if status == http::StatusCode::OK {
            // gRPC trailers-only responses: grpc-status appears in headers.
            // Note: for streaming gRPC, grpc-status is in trailers (not headers)
            // and will not be detected here. The circuit breaker handles streaming
            // gRPC failures via GrpcRetryPushbackClassifyEos.
            self.headers()
                .get("grpc-status")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u16>().ok())
                .and_then(|code| match code {
                    0 => None,                                   // OK
                    8 => Some(FailureHint::RateLimited),         // RESOURCE_EXHAUSTED
                    14 => Some(FailureHint::ServiceUnavailable), // UNAVAILABLE
                    _ => Some(FailureHint::InternalError),       // Other non-zero
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
        if self.status() == http::StatusCode::OK {
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
        if self.status() == http::StatusCode::OK {
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

/// Connection tuples (Connection, Metadata) used by TCP/TLS paths never indicate failures.
/// This is meant for MakeConnection services that return `(I, M)` tuples.
impl<C, M> ResponseFailureHint for (C, M) {}

/// TCP streams never indicate failures (no HTTP status codes).
impl ResponseFailureHint for tokio::net::TcpStream {}

/// Duplex streams (used in testing) never indicate failures.
impl ResponseFailureHint for tokio::io::DuplexStream {}

/// Mock IO streams never indicate failures.
#[cfg(feature = "tokio-test")]
impl ResponseFailureHint for tokio_test::io::Mock {}

/// Amplification factor for Retry-After penalties.
///
/// When a 429 or 503 response includes a Retry-After header, the load biaser injects
/// an amplified penalty so it remains meaningful through the server's requested
/// avoidance window. The injected value is:
///
///   `penalty_secs * RETRY_AFTER_PENALTY_FACTOR * exp(retry_after / penalty_decay)`
///
/// which decays via EWMA to `penalty_secs * RETRY_AFTER_PENALTY_FACTOR` at
/// exactly `t = retry_after`. The factor controls how aggressively the endpoint
/// is avoided near the Retry-After deadline:
///
/// At 1.0 the endpoint is fully avoided (~0% traffic) through the window and 46s
/// beyond -- too aggressive for early-recovery discovery. At 0.1, traffic leaks
/// well before the deadline. We use 0.5: the penalty at deadline is penalty_secs/2
/// (roughly 50x healthy load in a 5-endpoint pool), which lets occasional probes
/// through in the second half while still exceeding a plain failure penalty for
/// RA >= ~7s. For pools with 100+ endpoints the factor barely matters because P2C
/// random pair selection already makes the penalized endpoint unlikely to be drawn.
///
/// Sending occasional probe traffic during the window is valuable: the endpoint
/// may recover early, or a fresh 429 or 503 with a different RA provides updated
/// information. The circuit breaker (when enabled) provides a strict backoff
/// window, ie. `max(backoff, retry_after)`.
const RETRY_AFTER_PENALTY_FACTOR: f64 = 0.5;

/// Configuration for LoadBiaser behavior.
#[derive(Clone, Debug)]
pub struct LoadBiaserConfig {
    /// Default RTT to use when no measurements are available
    pub default_rtt: Duration,

    /// Decay duration for the RTT EWMA.
    /// Controls how quickly RTT estimates adapt to changing latency
    pub rtt_decay: Duration,

    /// The penalty value to inject on failure responses (429, 503, 5xx) in seconds
    pub penalty_secs: f64,

    /// Decay duration for the penalty EWMA.
    /// Controls how quickly the penalty decays after a failure response
    pub penalty_decay: Duration,

    /// Whether load biasing penalties are enabled. When false, only RTT tracking
    /// is active (PeakEwma equivalent).
    pub enabled: bool,

    /// Maximum Retry-After duration to honor. Clamped to this value.
    pub max_duration: Duration,
}

impl Default for LoadBiaserConfig {
    fn default() -> Self {
        Self {
            default_rtt: Duration::from_secs(1),
            rtt_decay: Duration::from_secs(10),
            // 5 second penalty on failure responses (429, 503, 5xx)
            penalty_secs: 5.0,
            // 10 second decay for penalty - mostly gone after ~30 seconds
            penalty_decay: Duration::from_secs(10),
            enabled: false,
            max_duration: DEFAULT_RETRY_AFTER_MAX_DURATION,
        }
    }
}

/// Shared per-endpoint state behind a single `Arc`.
///
/// Combines the EWMA trackers (under a mutex for atomic RTT+penalty reads),
/// the in-flight request counter, and the immutable config fields that every
/// response future needs. One allocation per endpoint instead of two, and
/// futures carry a single `Arc` clone instead of copying config fields.
#[derive(Debug)]
struct SharedState {
    /// RTT and penalty EWMAs; read-locked in load(), write-locked in poll()
    metrics: RwLock<LoadMetrics>,
    /// Count of in-flight requests (up on call, down on response)
    pending: AtomicU32,
    /// Penalty value to inject on failure responses (in seconds)
    penalty_secs: f64,
    /// Whether penalty injection is enabled
    enabled: bool,
    /// Maximum Retry-After duration to honor (clamped)
    max_duration: Duration,
    /// Decay duration for the penalty EWMA (used to amplify Retry-After penalties)
    penalty_decay: Duration,
}

#[derive(Debug)]
struct LoadMetrics {
    /// EWMA RTT tracking, updated on each response
    rtt: Ewma,
    /// EWMA penalty tracking, updated on failure responses (429, 503, 5xx)
    penalty: Ewma,
}

/// A service wrapper that tracks RTT and biases load metrics based on failure responses.
///
/// `LoadBiaser` provides load metrics for P2C load balancing by tracking request latency
/// (RTT) via EWMA, in-flight requests, and by injecting penalties when failure responses
/// (429, 503, 5xx) are detected.
///
/// The `load()` method returns `max(rtt * (pending + 1), penalty)`, causing P2C to
/// prefer endpoints with lower latency and fewer in-flight requests, while avoiding
/// rate-limited endpoints.
#[derive(Debug)]
pub struct LoadBiaser<S> {
    inner: S,
    /// Per-endpoint metrics, pending counter, and penalty config
    shared: Arc<SharedState>,
    config: LoadBiaserConfig,
}

/// A `NewService` implementation that creates `LoadBiaser` wrappers.
#[derive(Debug)]
pub struct NewLoadBiaser<N, Req> {
    inner: N,
    config: LoadBiaserConfig,
    _marker: PhantomData<fn(Req)>,
}

/// Response future that tracks RTT and checks for failure responses.
///
/// When the inner future completes we store the RTT based on elapsed time since the
/// request started, decrement the pending counter, and if the response indicates a
/// failure (using the `ResponseFailureHint` trait) we inject a penalty.
#[pin_project(PinnedDrop)]
pub struct LoadBiaserFuture<F, Rsp> {
    #[pin]
    inner: F,
    /// Request start instant for RTT calculation
    start: Instant,
    /// Shared endpoint state (metrics, pending counter, penalty config)
    shared: Arc<SharedState>,
    /// Whether we've already decremented pending
    completed: bool,
    /// Marker for the response type (`ResponseFailureHint` bound)
    _response: PhantomData<fn() -> Rsp>,
}

impl<S> LoadBiaser<S> {
    /// Creates a new `LoadBiaser` wrapping the given service.
    pub fn new(inner: S, mut config: LoadBiaserConfig) -> Self {
        if config.penalty_secs.is_nan() || config.penalty_secs < 0.0 {
            tracing::warn!(
                penalty_secs = config.penalty_secs,
                "penalty_secs is NaN or negative, clamping to 0.0"
            );
            config.penalty_secs = 0.0;
        }
        if config.penalty_decay < MIN_DECAY {
            tracing::warn!(
                penalty_decay = ?config.penalty_decay,
                min = ?MIN_DECAY,
                "penalty_decay below minimum, will be clamped by EWMA constructor"
            );
        }
        if config.rtt_decay < MIN_DECAY {
            tracing::warn!(
                rtt_decay = ?config.rtt_decay,
                min = ?MIN_DECAY,
                "rtt_decay below minimum, will be clamped by EWMA constructor"
            );
        }
        let now = Instant::now();
        let shared = Arc::new(SharedState {
            metrics: RwLock::new(LoadMetrics {
                // Initialize RTT with default_rtt (not INFINITY) so that fresh
                // endpoints have comparable load to unmeasured ones. This matches
                // Tower's PeakEwma behavior and prevents P2C from permanently
                // preferring endpoints whose first request happened to be fast.
                rtt: Ewma::new_with_value(config.rtt_decay, now, config.default_rtt.as_secs_f64()),
                penalty: Ewma::new(config.penalty_decay, now),
            }),
            pending: AtomicU32::new(0),
            penalty_secs: config.penalty_secs,
            enabled: config.enabled,
            max_duration: config.max_duration,
            penalty_decay: config.penalty_decay,
        });
        Self {
            inner,
            shared,
            config,
        }
    }

    /// Returns a reference to the inner service.
    pub fn get_ref(&self) -> &S {
        &self.inner
    }
}

impl<S: Clone> Clone for LoadBiaser<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            shared: self.shared.clone(),
            config: self.config.clone(),
        }
    }
}

impl<S> Load for LoadBiaser<S> {
    type Metric = f64;

    fn load(&self) -> Self::Metric {
        let pending = self.shared.pending.load(Ordering::Acquire);
        let now = Instant::now();

        let (rtt, penalty_val) = {
            let metrics = self.shared.metrics.read();
            (metrics.rtt.get_at(now), metrics.penalty.get_at(now))
        };

        let penalty = if penalty_val.is_infinite() {
            0.0
        } else {
            penalty_val
        };

        // Load = RTT * (pending + 1), minimum will be `penalty`
        // The +1 ensures idle endpoints have some load based on RTT
        let base = rtt * f64::from(pending.saturating_add(1));
        let load = f64::max(base, penalty);

        tracing::trace!(
            rtt_secs = rtt,
            pending = pending,
            penalty_secs = penalty,
            load = load,
            "LoadBiaser::load"
        );

        load
    }
}

impl<S, Req> Service<Req> for LoadBiaser<S>
where
    S: Service<Req>,
    S::Response: ResponseFailureHint,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = LoadBiaserFuture<S::Future, S::Response>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let prev = self.shared.pending.fetch_add(1, Ordering::AcqRel);
        debug_assert!(prev < u32::MAX, "pending counter overflow");

        LoadBiaserFuture {
            inner: self.inner.call(req),
            start: Instant::now(),
            shared: self.shared.clone(),
            completed: false,
            _response: PhantomData,
        }
    }
}

impl<N, Req> NewLoadBiaser<N, Req> {
    /// Creates a new `NewLoadBiaser` with the given configuration.
    pub fn new(config: LoadBiaserConfig, inner: N) -> Self {
        Self {
            inner,
            config,
            _marker: PhantomData,
        }
    }
}

impl<N: Clone, Req> Clone for NewLoadBiaser<N, Req> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            config: self.config.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T, N, S, Req> NewService<T> for NewLoadBiaser<N, Req>
where
    N: NewService<T, Service = S>,
{
    type Service = LoadBiaser<S>;

    fn new_service(&self, target: T) -> LoadBiaser<S> {
        LoadBiaser::new(self.inner.new_service(target), self.config.clone())
    }
}

impl<F, Rsp, E> Future for LoadBiaserFuture<F, Rsp>
where
    F: Future<Output = Result<Rsp, E>>,
    Rsp: ResponseFailureHint,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let mut result = ready!(this.inner.poll(cx));
        let shared = &**this.shared;

        let now = Instant::now();
        let elapsed = now.saturating_duration_since(*this.start).as_secs_f64();

        // Parse rate limit hint while we have &mut access to the response (when load
        // biasing is enabled).
        if shared.enabled {
            if let Ok(ref mut resp) = result {
                resp.attach_parsed_rate_limit_hint();
            }
        }

        {
            let mut metrics = shared.metrics.write();
            if let Ok(ref resp) = result {
                // Update RTT on all HTTP responses (including 429, 503, 5xx).
                // Only transport-level errors (connection refused, resets) skip
                // this update, because broken endpoints fail fast and would bias
                // P2C toward sending more traffic to the broken endpoint.
                metrics.rtt.add_peak(elapsed, now);

                if shared.enabled {
                    if let Some(hint) = resp.failure_hint() {
                        let base_penalty = shared.penalty_secs;

                        // For rate-limited and service-unavailable responses, amplify
                        // the penalty using the Retry-After hint so it remains meaningful
                        // through the server's requested avoidance window.
                        let penalty_val = match hint {
                            FailureHint::RateLimited | FailureHint::ServiceUnavailable => {
                                match resp.rate_limit_hint(shared.max_duration) {
                                    Some(ra) if ra.as_secs_f64() > 0.0 => {
                                        let decay_secs =
                                            shared.penalty_decay.max(MIN_DECAY).as_secs_f64();
                                        let amplified = base_penalty
                                            * RETRY_AFTER_PENALTY_FACTOR
                                            * (ra.as_secs_f64() / decay_secs).exp();
                                        amplified.min(1e12)
                                    }
                                    _ => base_penalty,
                                }
                            }
                            FailureHint::InternalError => base_penalty,
                        };

                        tracing::debug!(
                            penalty_secs = penalty_val,
                            rtt_secs = elapsed,
                            ?hint,
                            "Detected failure response - injecting load penalty"
                        );
                        metrics.penalty.add_peak(penalty_val, now);
                    }
                }
            }
        }

        shared.pending.fetch_sub(1, Ordering::Release);
        *this.completed = true;

        Poll::Ready(result)
    }
}

#[pin_project::pinned_drop]
impl<F, Rsp> PinnedDrop for LoadBiaserFuture<F, Rsp> {
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        // Only decrement if we haven't already done so in poll().
        // Avoids leaking the pending count upon cancellation.
        if !*this.completed {
            this.shared.pending.fetch_sub(1, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use tokio::time;

    impl<S> LoadBiaser<S> {
        pub fn get_rtt(&self) -> f64 {
            self.shared.metrics.read().rtt.get()
        }

        pub fn get_penalty(&self) -> f64 {
            self.shared.metrics.read().penalty.get()
        }

        pub fn get_pending(&self) -> u32 {
            self.shared.pending.load(Ordering::Acquire)
        }

        pub fn inject_penalty(&self, penalty_secs: f64) {
            self.shared
                .metrics
                .write()
                .penalty
                .add_peak(penalty_secs, Instant::now());
        }

        pub fn inject_rtt(&self, rtt_secs: f64) {
            self.shared
                .metrics
                .write()
                .rtt
                .add_peak(rtt_secs, Instant::now());
        }
    }

    // Mock service for testing returning a specific HTTP status.
    #[derive(Clone)]
    struct MockService {
        status: http::StatusCode,
    }

    impl MockService {
        fn new(status: http::StatusCode) -> Self {
            Self { status }
        }
    }

    impl Service<()> for MockService {
        type Response = http::Response<&'static str>;
        type Error = Infallible;
        type Future = futures::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: ()) -> Self::Future {
            let resp = http::Response::builder()
                .status(self.status)
                .body("test")
                .unwrap();
            futures::future::ready(Ok(resp))
        }
    }

    // Mock service that always returns an error.
    #[derive(Clone)]
    struct ErrorService;

    impl Service<()> for ErrorService {
        type Response = http::Response<&'static str>;
        type Error = &'static str;
        type Future = futures::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: ()) -> Self::Future {
            futures::future::ready(Err("connection refused"))
        }
    }

    fn test_config() -> LoadBiaserConfig {
        LoadBiaserConfig {
            default_rtt: Duration::from_millis(100), // 0.1s default
            rtt_decay: Duration::from_secs(10),
            penalty_secs: 5.0,
            penalty_decay: Duration::from_secs(10),
            enabled: true,
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

        // Inject a known RTT
        time::sleep(Duration::from_millis(1)).await;
        biaser.inject_rtt(0.05); // 50ms

        // RTT * (0 + 1) = 0.05
        let load_idle = biaser.load();

        // Start a request (not awaited yet)
        // Increment pending to simulate in-flight requests
        biaser.shared.pending.fetch_add(1, Ordering::AcqRel);

        // RTT * (1 + 1) = 0.05 * 2 = 0.1
        let load_one_pending = biaser.load();

        // Increment again
        biaser.shared.pending.fetch_add(1, Ordering::AcqRel);

        // RTT * (2 + 1) = 0.05 * 3 = 0.15
        let load_two_pending = biaser.load();

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
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_rtt_tracked_after_request() {
        let inner = MockService::new(http::StatusCode::OK);
        let mut biaser = LoadBiaser::new(inner, test_config());

        // Advance time so EWMA accepts updates
        time::sleep(Duration::from_millis(1)).await;

        // RTT should start at default_rtt (0.1s) before any requests
        let initial_rtt = biaser.get_rtt();
        assert!(
            (initial_rtt - 0.1).abs() < 0.01,
            "RTT should start at default_rtt (0.1s), got: {initial_rtt}"
        );

        // Make a request (will record RTT)
        let _ = biaser.call(()).await;

        // RTT should now reflect the actual request latency
        let rtt = biaser.get_rtt();
        assert!(
            rtt < initial_rtt,
            "RTT should decrease after a fast request"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_429_injects_penalty() {
        let inner = MockService::new(http::StatusCode::TOO_MANY_REQUESTS);
        let mut biaser = LoadBiaser::new(inner, test_config());

        time::sleep(Duration::from_millis(1)).await;

        // Penalty should be infinite (none) before we get requests.
        assert!(biaser.get_penalty().is_infinite());

        // Make a request that returns 429.
        let _ = biaser.call(()).await;

        // A penalty should be injected (5 seconds) after seeing a 429
        let penalty = biaser.get_penalty();
        assert!(
            (penalty - 5.0).abs() < 0.1,
            "penalty should be ~5s after 429: {}",
            penalty
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

        // Penalty should still be infinite (none).
        assert!(
            biaser.get_penalty().is_infinite(),
            "penalty should not be injected for 200"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_penalty_decays_over_time() {
        let inner = MockService::new(http::StatusCode::TOO_MANY_REQUESTS);
        // Use a custom config with default_rtt=1ms (0.001s) so load() tracks penalty directly.
        // inject_rtt(0.001) would be a no-op here: add_peak() only replaces when the new
        // value exceeds the decayed current value, so 0.001 < 0.1 (test_config default)
        // would fail the peak check and fall through to add() which no-ops at ts==self.timestamp.
        let config = LoadBiaserConfig {
            default_rtt: Duration::from_millis(1),
            ..test_config()
        };
        let mut biaser = LoadBiaser::new(inner, config);

        time::sleep(Duration::from_millis(1)).await;

        // Trigger penalty via 429 response
        let _ = biaser.call(()).await;

        // After call completes, pending is back to 0 (LoadBiaserFuture::poll decrements
        // before returning Poll::Ready), so load = max(rtt * 1, penalty).
        assert_eq!(
            biaser.get_pending(),
            0,
            "pending should be 0 after call completes"
        );

        // load() calls get_at(now) which projects the decayed penalty.
        // Immediately after injection: penalty ~5.0, rtt ~0.001, so load ~5.0
        let load_before = biaser.load();
        assert!(
            load_before > 4.0,
            "load before decay should be dominated by the injected 429 penalty: {}",
            load_before
        );

        // Advance time by one decay period (10s).
        // Penalty decays: 5.0 * e^(-10/10) ~ 1.839
        time::sleep(Duration::from_secs(10)).await;

        // load() projects the decayed penalty at the new timestamp.
        // No EWMA mutation needed since this is pure projection.
        let load_after = biaser.load();
        assert!(
            load_after > 1.0,
            "load after one decay period should still be dominated by decayed penalty: {}",
            load_after
        );
        assert!(
            load_after < 2.5,
            "load after one decay period should reflect substantial decay (5.0 * e^-1 ~ 1.839): {}",
            load_after
        );
        assert!(
            load_after < load_before,
            "penalty should decay over time as observed through load(): {} < {}",
            load_after,
            load_before
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_load_is_max_of_rtt_based_and_penalty() {
        let inner = MockService::new(http::StatusCode::OK);
        let biaser = LoadBiaser::new(inner, test_config());

        time::sleep(Duration::from_millis(1)).await;

        // Inject a high RTT (10 seconds)
        biaser.inject_rtt(10.0);

        // Inject a lower penalty (1 second)
        biaser.inject_penalty(1.0);

        // Load should be RTT-based since it's higher: 10 * 1 = 10
        let load = biaser.load();
        assert!(
            (load - 10.0).abs() < 0.1,
            "load should be RTT-based when higher: {}",
            load
        );

        // Now inject a very high penalty
        biaser.inject_penalty(20.0);

        // Load should be penalty since it's higher
        let load = biaser.load();
        assert!(
            (load - 20.0).abs() < 0.1,
            "load should be penalty when higher: {}",
            load
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_clone_shares_state() {
        let inner = MockService::new(http::StatusCode::TOO_MANY_REQUESTS);
        let mut biaser1 = LoadBiaser::new(inner.clone(), test_config());
        let biaser2 = biaser1.clone();

        time::sleep(Duration::from_millis(1)).await;

        // Trigger penalty on biaser1
        let _ = biaser1.call(()).await;

        // biaser2 should see the same penalty (shared state)
        assert_eq!(biaser1.get_penalty(), biaser2.get_penalty());
        assert_eq!(biaser1.load(), biaser2.load());
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_pending_decremented_on_completion() {
        let inner = MockService::new(http::StatusCode::OK);
        let mut biaser = LoadBiaser::new(inner, test_config());

        assert_eq!(biaser.get_pending(), 0, "pending should start at 0");

        // Start a request, pending increments
        let fut = biaser.call(());
        assert_eq!(
            biaser.get_pending(),
            1,
            "pending should be 1 during request"
        );

        // Complete the request, pending decrements
        let _ = fut.await;
        assert_eq!(
            biaser.get_pending(),
            0,
            "pending should be 0 after completion"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_rtt_not_updated_on_error() {
        let inner = ErrorService;
        let mut biaser = LoadBiaser::new(inner, test_config());

        time::sleep(Duration::from_millis(1)).await;

        // RTT should start at default_rtt before any requests
        let initial_rtt = biaser.get_rtt();
        assert!(
            (initial_rtt - 0.1).abs() < 0.01,
            "RTT should start at default_rtt (0.1s), got: {initial_rtt}"
        );

        // Make a request that returns an error
        let _ = biaser.call(()).await;

        // RTT should remain at default_rtt because error responses don't
        // update RTT. This prevents P2C from routing more traffic to broken
        // endpoints that fail fast (appearing "fast" to the load metric).
        let rtt_after_error = biaser.get_rtt();
        assert!(
            (rtt_after_error - initial_rtt).abs() < 0.01,
            "RTT should not change on error responses: {rtt_after_error} vs {initial_rtt}"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_pending_decremented_on_cancellation() {
        use tokio::sync::oneshot;

        // Build a service whose response future blocks on a oneshot receiver.
        // This lets us drop the future mid-flight to test PinnedDrop.
        struct DelayedService {
            rx: Option<oneshot::Receiver<http::Response<&'static str>>>,
        }

        impl Service<()> for DelayedService {
            type Response = http::Response<&'static str>;
            type Error = Infallible;
            type Future = DelayedFuture;

            fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }

            fn call(&mut self, _: ()) -> Self::Future {
                DelayedFuture {
                    rx: self.rx.take().expect("called more than once"),
                }
            }
        }

        struct DelayedFuture {
            rx: oneshot::Receiver<http::Response<&'static str>>,
        }

        impl std::future::Future for DelayedFuture {
            type Output = Result<http::Response<&'static str>, Infallible>;

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                match Pin::new(&mut self.rx).poll(cx) {
                    Poll::Ready(Ok(resp)) => Poll::Ready(Ok(resp)),
                    Poll::Ready(Err(_)) => {
                        // Sender dropped. Return a default response
                        let resp = http::Response::builder()
                            .status(http::StatusCode::OK)
                            .body("cancelled")
                            .unwrap();

                        Poll::Ready(Ok(resp))
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
        }

        let (tx, rx) = oneshot::channel();
        let inner = DelayedService { rx: Some(rx) };
        let mut biaser = LoadBiaser::new(inner, test_config());

        assert_eq!(biaser.get_pending(), 0);

        // Initiate the request, pending increments in Service::call()
        let fut = biaser.call(());
        assert_eq!(biaser.get_pending(), 1);

        // Drop the future without completing it. The oneshot receiver
        // never receives a value, so the inner future is still pending.
        // PinnedDrop must decrement the pending count.
        drop(fut);

        assert_eq!(
            biaser.get_pending(),
            0,
            "PinnedDrop should decrement pending on cancellation"
        );

        // tx is unused. Dropping it here is fine, it just closes the channel.
        drop(tx);
    }

    #[test]
    fn default_max_duration_matches_client_policy() {
        // Enforces the invariant at LoadBiaserConfig::default():
        // max_duration must match the local DEFAULT_RETRY_AFTER_MAX_DURATION constant.
        // Production code reads the constant directly via EwmaConfig::to_load_biaser_config;
        // this assertion keeps the test-only default() in sync.
        assert_eq!(
            LoadBiaserConfig::default().max_duration,
            DEFAULT_RETRY_AFTER_MAX_DURATION,
            "LoadBiaserConfig::default().max_duration must match \
             DEFAULT_RETRY_AFTER_MAX_DURATION (300s)"
        );
    }

    // Mock service returning an HTTP error with a Retry-After header.
    // Parameterized by status code to avoid duplicating 429 and 503 variants.
    #[derive(Clone)]
    struct RetryAfterService {
        status: http::StatusCode,
        retry_after_secs: u64,
    }

    impl Service<()> for RetryAfterService {
        type Response = http::Response<&'static str>;
        type Error = Infallible;
        type Future = futures::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: ()) -> Self::Future {
            let resp = http::Response::builder()
                .status(self.status)
                .header(http::header::RETRY_AFTER, self.retry_after_secs.to_string())
                .body("retry-after response")
                .unwrap();

            futures::future::ready(Ok(resp))
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_429_with_retry_after_uses_adaptive_penalty() {
        let inner = RetryAfterService {
            status: http::StatusCode::TOO_MANY_REQUESTS,
            retry_after_secs: 30,
        };
        let mut biaser = LoadBiaser::new(inner, test_config());

        time::sleep(Duration::from_millis(1)).await;

        // Make a request that returns 429 with Retry-After: 30
        let _ = biaser.call(()).await;

        // Amplified: penalty_secs * FACTOR * exp(RA/decay) = 5.0 * 0.5 * e^3
        let expected = 5.0_f64 * RETRY_AFTER_PENALTY_FACTOR * (30.0_f64 / 10.0_f64).exp();
        let penalty = biaser.get_penalty();

        assert!(
            (penalty - expected).abs() < 1.0,
            "penalty should be ~{expected:.1} (amplified), got: {}",
            penalty
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_429_with_retry_after_clamped_to_max() {
        let inner = RetryAfterService {
            status: http::StatusCode::TOO_MANY_REQUESTS,
            retry_after_secs: 600,
        };
        let config = LoadBiaserConfig {
            max_duration: DEFAULT_RETRY_AFTER_MAX_DURATION,
            ..test_config()
        };
        let mut biaser = LoadBiaser::new(inner, config);

        time::sleep(Duration::from_millis(1)).await;

        // Make a request that returns 429 with Retry-After: 600, clamped to 300s
        let _ = biaser.call(()).await;

        // Amplified: penalty_secs * FACTOR * exp(clamped_RA/decay) = 5.0 * 0.5 * e^30
        // exceeds the 1e12 penalty clamp, so the actual penalty is clamped.
        let unclamped = 5.0_f64 * RETRY_AFTER_PENALTY_FACTOR * (300.0_f64 / 10.0_f64).exp();
        let expected = unclamped.min(1e12);
        let penalty = biaser.get_penalty();

        assert!(
            (penalty - expected).abs() / expected < 0.01,
            "penalty should be ~{expected:.0} (amplified, clamped RA=300), got: {}",
            penalty
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
    async fn test_503_injects_full_penalty() {
        let inner = MockService::new(http::StatusCode::SERVICE_UNAVAILABLE);
        let mut biaser = LoadBiaser::new(inner, test_config());

        time::sleep(Duration::from_millis(1)).await;

        let _ = biaser.call(()).await;
        let penalty = biaser.get_penalty();

        // test_config has penalty_secs = 5.0
        assert!(
            (penalty - 5.0).abs() < 0.1,
            "503 should inject full penalty (~5.0s), got: {penalty}"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_503_with_retry_after_uses_adaptive_penalty() {
        let inner = RetryAfterService {
            status: http::StatusCode::SERVICE_UNAVAILABLE,
            retry_after_secs: 30,
        };
        let mut biaser = LoadBiaser::new(inner, test_config());

        time::sleep(Duration::from_millis(1)).await;

        // Make a request that returns 503 with Retry-After: 30
        let _ = biaser.call(()).await;

        // Same amplification formula as 429:
        // penalty_secs * FACTOR * exp(RA/decay) = 5.0 * 0.5 * e^3
        let expected = 5.0_f64 * RETRY_AFTER_PENALTY_FACTOR * (30.0_f64 / 10.0_f64).exp();
        let penalty = biaser.get_penalty();
        assert!(
            (penalty - expected).abs() < 1.0,
            "503+RA penalty should be ~{expected:.1} (amplified), got: {penalty}"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_503_with_retry_after_clamped_to_max() {
        let inner = RetryAfterService {
            status: http::StatusCode::SERVICE_UNAVAILABLE,
            retry_after_secs: 600,
        };
        let config = LoadBiaserConfig {
            max_duration: DEFAULT_RETRY_AFTER_MAX_DURATION,
            ..test_config()
        };
        let mut biaser = LoadBiaser::new(inner, config);

        time::sleep(Duration::from_millis(1)).await;

        // Make a request that returns 503 with Retry-After: 600, clamped to 300s
        let _ = biaser.call(()).await;

        // Amplified with clamped RA:
        // penalty_secs * FACTOR * exp(clamped_RA/decay) = 5.0 * 0.5 * e^30
        // exceeds the 1e12 penalty clamp, so the actual penalty is clamped.
        let unclamped = 5.0_f64 * RETRY_AFTER_PENALTY_FACTOR * (300.0_f64 / 10.0_f64).exp();
        let expected = unclamped.min(1e12);
        let penalty = biaser.get_penalty();
        assert!(
            (penalty - expected).abs() / expected < 0.01,
            "503+RA penalty should be ~{expected:.0} (amplified, clamped RA=300), got: {penalty}"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_429_and_503_retry_after_produce_identical_penalty() {
        // 429 path: RetryAfterService returns TOO_MANY_REQUESTS with Retry-After: 30
        let inner_429 = RetryAfterService {
            status: http::StatusCode::TOO_MANY_REQUESTS,
            retry_after_secs: 30,
        };
        let mut biaser_429 = LoadBiaser::new(inner_429, test_config());

        // 503 path: RetryAfterService returns SERVICE_UNAVAILABLE with Retry-After: 30
        let inner_503 = RetryAfterService {
            status: http::StatusCode::SERVICE_UNAVAILABLE,
            retry_after_secs: 30,
        };
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
    async fn test_500_injects_penalty() {
        let inner = MockService::new(http::StatusCode::INTERNAL_SERVER_ERROR);
        let mut biaser = LoadBiaser::new(inner, test_config());

        time::sleep(Duration::from_millis(1)).await;

        let _ = biaser.call(()).await;
        let penalty = biaser.get_penalty();

        // test_config has penalty_secs = 5.0
        assert!(
            (penalty - 5.0).abs() < 0.1,
            "500 should inject full penalty (~5.0s), got: {penalty}"
        );
    }

    // Mock service that returns HTTP 200 with a `grpc-status` header,
    // simulating a gRPC trailers-only error response.
    #[derive(Clone)]
    struct GrpcErrorService {
        grpc_status: u16,
    }

    impl Service<()> for GrpcErrorService {
        type Response = http::Response<&'static str>;
        type Error = Infallible;
        type Future = futures::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: ()) -> Self::Future {
            let resp = http::Response::builder()
                .status(http::StatusCode::OK)
                .header("grpc-status", self.grpc_status.to_string())
                .body("grpc error")
                .unwrap();

            futures::future::ready(Ok(resp))
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_grpc_resource_exhausted_injects_penalty() {
        let inner = GrpcErrorService { grpc_status: 8 }; // RESOURCE_EXHAUSTED
        let mut biaser = LoadBiaser::new(inner, test_config());

        time::sleep(Duration::from_millis(1)).await;

        let _ = biaser.call(()).await;
        // gRPC RESOURCE_EXHAUSTED maps to FailureHint::RateLimited,
        // which injects the full penalty_secs (5.0).
        let penalty = biaser.get_penalty();

        assert!(
            (penalty - 5.0).abs() < 0.1,
            "gRPC RESOURCE_EXHAUSTED should inject full penalty (~5s), got: {penalty}"
        );
    }

    async fn assert_disabled_no_penalty<S>(mut biaser: LoadBiaser<S>, label: &str)
    where
        S: Service<(), Response = http::Response<&'static str>, Error = Infallible>,
    {
        time::sleep(Duration::from_millis(1)).await;
        let _ = biaser.call(()).await;
        assert!(
            biaser.get_penalty().is_infinite(),
            "penalty should not be injected when disabled for {label}: {}",
            biaser.get_penalty()
        );
    }

    fn disabled_config() -> LoadBiaserConfig {
        LoadBiaserConfig {
            enabled: false,
            ..test_config()
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_429_disabled_no_penalty() {
        let inner = MockService::new(http::StatusCode::TOO_MANY_REQUESTS);
        assert_disabled_no_penalty(LoadBiaser::new(inner, disabled_config()), "429").await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_503_disabled_no_penalty() {
        let inner = MockService::new(http::StatusCode::SERVICE_UNAVAILABLE);
        assert_disabled_no_penalty(LoadBiaser::new(inner, disabled_config()), "503").await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_500_disabled_no_penalty() {
        let inner = MockService::new(http::StatusCode::INTERNAL_SERVER_ERROR);
        assert_disabled_no_penalty(LoadBiaser::new(inner, disabled_config()), "500").await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_grpc_resource_exhausted_disabled_no_penalty() {
        let inner = GrpcErrorService { grpc_status: 8 };
        assert_disabled_no_penalty(
            LoadBiaser::new(inner, disabled_config()),
            "gRPC RESOURCE_EXHAUSTED (status 8)",
        )
        .await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_grpc_unavailable_disabled_no_penalty() {
        let inner = GrpcErrorService { grpc_status: 14 };
        assert_disabled_no_penalty(
            LoadBiaser::new(inner, disabled_config()),
            "gRPC UNAVAILABLE (status 14)",
        )
        .await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_grpc_internal_disabled_no_penalty() {
        let inner = GrpcErrorService { grpc_status: 13 };
        assert_disabled_no_penalty(
            LoadBiaser::new(inner, disabled_config()),
            "gRPC INTERNAL (status 13)",
        )
        .await;
    }
}
