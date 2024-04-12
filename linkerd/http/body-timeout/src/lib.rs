#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use futures::prelude::*;
use http::{HeaderMap, HeaderValue};
use http_body::Body;
use linkerd_error::Error;
use pin_project::pin_project;
use prometheus_client::metrics::{counter::Counter, family::Family};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::time;

pub struct TimeoutRequestProgress<S> {
    inner: S,
    timeout: time::Duration,
    metrics: Metrics,
}

pub struct TimeoutResponseProgress<S> {
    inner: S,
    timeout: time::Duration,
    metrics: Metrics,
}

#[derive(Clone, Debug)]
pub struct MetricFamilies<L: Clone> {
    timeouts: Family<L, Counter>,
}

#[derive(Clone, Debug, Default)]
pub struct Metrics {
    timeouts: Counter,
}

impl<L> MetricFamilies<L>
where
    L: prometheus_client::encoding::EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone + Send + Sync + 'static,
{
    /// Registers metrics families (i.e. with labels).
    pub fn register(registry: &mut prometheus_client::registry::Registry) -> Self {
        let timeouts = Family::default();
        registry.register(
            "stream_progress_timeouts",
            "The number of progress timeouts that have triggered on HTTP resopnse streams",
            timeouts.clone(),
        );
        Self { timeouts }
    }

    pub fn metrics(&self, labels: &L) -> Metrics {
        let timeouts = self.timeouts.get_or_create(labels).clone();
        Metrics { timeouts }
    }
}

impl<L> Default for MetricFamilies<L>
where
    L: prometheus_client::encoding::EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone + Send + Sync + 'static,
{
    fn default() -> Self {
        Self {
            timeouts: Family::default(),
        }
    }
}

impl Metrics {
    /// Registers singleton metrics (i.e. without labels).
    pub fn register(registry: &mut prometheus_client::registry::Registry) -> Self {
        let timeouts = Counter::default();
        registry.register(
            "stream_progress_timeouts",
            "The number of progress timeouts that have triggered on HTTP resopnse streams",
            timeouts.clone(),
        );
        Self { timeouts }
    }

    /// Returns the current value of the timeouts counter. For tests.
    pub fn timeouts(&self) -> u64 {
        self.timeouts.get()
    }
}

/// A [`Body`] that imposes a timeout on the amount of time the stream may be
/// stuck waiting for capacity.
#[derive(Debug)]
#[pin_project]
pub struct ProgressTimeoutBody<B> {
    #[pin]
    inner: B,
    sleep: Pin<Box<time::Sleep>>,
    timeout: time::Duration,
    is_pending: bool,
    metrics: Metrics,
}

#[derive(Debug, thiserror::Error)]
#[error("body progress timeout after {0:?}")]
pub struct BodyProgressTimeoutError(time::Duration);

// === impl TimeoutRequestProgress ===

impl<S> TimeoutRequestProgress<S> {
    pub fn new(metrics: Metrics, timeout: time::Duration, inner: S) -> Self {
        Self {
            inner,
            timeout,
            metrics,
        }
    }
}

impl<B, S> tower_service::Service<http::Request<B>> for TimeoutRequestProgress<S>
where
    S: tower_service::Service<http::Request<ProgressTimeoutBody<B>>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        self.inner
            .call(req.map(|b| ProgressTimeoutBody::new(self.metrics.clone(), self.timeout, b)))
    }
}

// === impl TimeoutResponseProgress ===

impl<S> TimeoutResponseProgress<S> {
    pub fn new(metrics: Metrics, timeout: time::Duration, inner: S) -> Self {
        Self {
            inner,
            timeout,
            metrics,
        }
    }
}

impl<Req, B, S> tower_service::Service<Req> for TimeoutResponseProgress<S>
where
    S: tower_service::Service<Req, Response = http::Response<B>>,
    S::Future: Send + 'static,
{
    type Response = http::Response<ProgressTimeoutBody<B>>;
    type Error = S::Error;
    type Future =
        Pin<Box<dyn std::future::Future<Output = Result<Self::Response, S::Error>> + Send>>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, req: Req) -> Self::Future {
        let timeout = self.timeout;
        let metrics = self.metrics.clone();
        self.inner
            .call(req)
            .map_ok(move |res| res.map(|b| ProgressTimeoutBody::new(metrics, timeout, b)))
            .boxed()
    }
}

// === impl ProgressTimeoutBody ===

impl<B> ProgressTimeoutBody<B> {
    pub fn new(metrics: Metrics, timeout: time::Duration, inner: B) -> Self {
        // Avoid overflows by capping MAX to roughly 30 years.
        const MAX: time::Duration = time::Duration::from_secs(86400 * 365 * 30);
        Self {
            inner,
            timeout: timeout.min(MAX),
            is_pending: false,
            sleep: Box::pin(time::sleep(MAX)),
            metrics,
        }
    }
}

impl<B: Default> Default for ProgressTimeoutBody<B> {
    fn default() -> Self {
        Self::new(Metrics::default(), time::Duration::MAX, B::default())
    }
}

impl<B> Body for ProgressTimeoutBody<B>
where
    B: Body + Send + 'static,
    B::Data: Send + 'static,
    B::Error: Into<Error>,
{
    type Data = B::Data;
    type Error = Error;

    #[inline]
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    #[inline]
    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let this = self.project();
        *this.is_pending = false;
        this.inner.poll_data(cx).map_err(Into::into)
    }

    #[inline]
    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<HeaderMap<HeaderValue>>, Self::Error>> {
        let this = self.project();
        *this.is_pending = false;
        this.inner.poll_trailers(cx).map_err(Into::into)
    }

    fn poll_progress(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = self.project();

        let _ = this.inner.poll_progress(cx).map_err(Into::into)?;

        if !*this.is_pending {
            this.sleep
                .as_mut()
                .reset(time::Instant::now() + *this.timeout);
            *this.is_pending = true;
        }

        futures::ready!(this.sleep.as_mut().poll(cx));

        tracing::debug!(timeout = ?this.timeout, "Body progress timed out");
        this.metrics.timeouts.inc();

        Poll::Ready(Err(::h2::Error::from(::h2::Reason::CANCEL).into()))
    }

    #[inline]
    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}
