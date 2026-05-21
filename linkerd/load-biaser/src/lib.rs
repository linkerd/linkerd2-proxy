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
pub struct CachedRateLimitHint(pub Duration);

/// Trait for extracting failure hints from responses.
///
/// This allows the load biaser to classify responses and apply appropriate
/// penalties. Default implementations return None (no failure detected),
/// which is appropriate for non-HTTP transports.
///
/// The trait splits rate limit hint access into two methods to avoid
/// requiring `&mut self` on the read path:
/// - `attach_parsed_rate_limit_hint(&mut self, max)`: parse and cache (needs `&mut`)
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
    /// The `_max` parameter is accepted for API symmetry with `rate_limit_hint(max)`
    /// but is intentionally unused. The raw uncapped value is cached so that each
    /// consumer can apply their own cap via `rate_limit_hint(max)`.
    fn attach_parsed_rate_limit_hint(&mut self, _max: Duration) {}

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

    fn attach_parsed_rate_limit_hint(&mut self, _max: Duration) {
        // Store the uncapped value. Each consumer applies their own cap via
        // rate_limit_hint(max).
        if let Some(d) = linkerd_http_classify::retry_after::parse_retry_after(
            self.status(),
            self.headers(),
            Duration::MAX,
        ) {
            self.extensions_mut().insert(CachedRateLimitHint(d));
            return;
        }
        // Try gRPC retry-pushback-ms (for trailers-only responses)
        if self.status() == http::StatusCode::OK {
            if let Some(d) = linkerd_http_classify::retry_after::parse_grpc_retry_pushback(
                self.headers(),
                Duration::MAX,
            ) {
                self.extensions_mut().insert(CachedRateLimitHint(d));
            }
        }
    }

    fn rate_limit_hint(&self, max: Duration) -> Option<Duration> {
        // Check cache first (from previous attach call), apply caller's cap
        if let Some(cached) = self.extensions().get::<CachedRateLimitHint>() {
            return Some(cached.0.min(max));
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
                resp.attach_parsed_rate_limit_hint(shared.max_duration);
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

