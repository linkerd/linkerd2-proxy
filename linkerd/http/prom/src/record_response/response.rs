use linkerd_error::Error;
use linkerd_http_box::BoxBody;
use linkerd_metrics::prom::Counter;
use linkerd_stack as svc;
use prometheus_client::{
    metrics::family::Family,
    registry::{Registry, Unit},
};
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{sync::oneshot, time};

use super::{DurationFamily, MkDurationHistogram, MkStreamLabel, StreamLabel};

/// Metrics type that tracks completed responses.
#[derive(Debug)]
pub struct ResponseMetrics<L: StreamLabel> {
    duration: DurationFamily<L::DurationLabels>,
    statuses: Family<L::StatusLabels, Counter>,
}

pub type NewResponseDuration<L, X, N> =
    super::NewRecordResponse<L, X, ResponseMetrics<<L as MkStreamLabel>::StreamLabel>, N>;

pub type RecordResponseDuration<L, S> =
    super::RecordResponse<L, ResponseMetrics<<L as MkStreamLabel>::StreamLabel>, S>;

/// Notifies the response body when the request body is flushed.
#[pin_project::pin_project(PinnedDrop)]
struct RequestBody<B> {
    #[pin]
    inner: B,
    flushed: Option<oneshot::Sender<time::Instant>>,
}

// === impl ResponseMetrics ===

impl<L> ResponseMetrics<L>
where
    L: StreamLabel,
{
    pub fn register(reg: &mut Registry, histo: impl IntoIterator<Item = f64>) -> Self {
        let duration =
            DurationFamily::new_with_constructor(MkDurationHistogram(histo.into_iter().collect()));
        reg.register_with_unit(
            "response_duration",
            "The time between request completion and response completion",
            Unit::Seconds,
            duration.clone(),
        );

        let statuses = Family::default();
        reg.register("response_statuses", "Completed responses", statuses.clone());

        Self { duration, statuses }
    }
}

#[cfg(feature = "test-util")]
impl<L> ResponseMetrics<L>
where
    L: StreamLabel,
{
    pub fn get_statuses(&self, labels: &L::StatusLabels) -> Counter {
        (*self.statuses.get_or_create(labels)).clone()
    }
}

impl<L: StreamLabel> Default for ResponseMetrics<L> {
    fn default() -> Self {
        Self {
            duration: DurationFamily::new_with_constructor(MkDurationHistogram(Arc::new([]))),
            statuses: Default::default(),
        }
    }
}

impl<L: StreamLabel> Clone for ResponseMetrics<L> {
    fn clone(&self) -> Self {
        Self {
            duration: self.duration.clone(),
            statuses: self.statuses.clone(),
        }
    }
}

impl<M, S> svc::Service<http::Request<BoxBody>> for RecordResponseDuration<M, S>
where
    M: MkStreamLabel,
    S: svc::Service<http::Request<BoxBody>, Response = http::Response<BoxBody>, Error = Error>,
{
    type Response = http::Response<BoxBody>;
    type Error = Error;
    type Future = super::ResponseFuture<M::StreamLabel, S::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<BoxBody>) -> Self::Future {
        // If there's a labeler, wrap the request body to record the time that
        // the respond flushes.
        let (parts, mut body) = req.into_parts();
        let state = if let Some(labeler) = self.labeler.mk_stream_labeler(&parts) {
            let (tx, start) = oneshot::channel();
            body = BoxBody::new(RequestBody {
                inner: body,
                flushed: Some(tx),
            });
            let ResponseMetrics { duration, statuses } = self.metric.clone();
            Some(super::ResponseState {
                labeler,
                start,
                duration,
                statuses,
            })
        } else {
            None
        };

        let req = http::Request::from_parts(parts, body);
        let inner = self.inner.call(req);
        super::ResponseFuture { state, inner }
    }
}

// === impl ResponseBody ===

impl<B> http_body::Body for RequestBody<B>
where
    B: http_body::Body,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, B::Error>>> {
        let mut this = self.project();
        let res = futures::ready!(this.inner.as_mut().poll_data(cx));
        if (*this.inner).is_end_stream() {
            if let Some(tx) = this.flushed.take() {
                let _ = tx.send(time::Instant::now());
            }
        }
        Poll::Ready(res)
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, B::Error>> {
        let this = self.project();
        let res = futures::ready!(this.inner.poll_trailers(cx));
        if let Some(tx) = this.flushed.take() {
            let _ = tx.send(time::Instant::now());
        }
        Poll::Ready(res)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}

#[pin_project::pinned_drop]
impl<B> PinnedDrop for RequestBody<B> {
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        if let Some(tx) = this.flushed.take() {
            let _ = tx.send(time::Instant::now());
        }
    }
}
