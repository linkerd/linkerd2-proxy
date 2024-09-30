use linkerd_error::Error;
use linkerd_http_box::BoxBody;
use linkerd_metrics::prom::Counter;
use linkerd_stack as svc;
use prometheus_client::{
    metrics::family::Family,
    registry::{Registry, Unit},
};
use std::{
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{sync::oneshot, time};

use super::{DurationFamily, MkDurationHistogram, MkStreamLabel, StreamLabel};

/// Metrics type that tracks completed requests.
#[derive(Debug)]
pub struct RequestMetrics<L: StreamLabel> {
    duration: DurationFamily<L::DurationLabels>,
    statuses: Family<L::StatusLabels, Counter>,
}

pub type NewRequestDuration<L, X, N> =
    super::NewRecordResponse<L, X, RequestMetrics<<L as MkStreamLabel>::StreamLabel>, N>;

pub type RecordRequestDuration<L, S> =
    super::RecordResponse<L, RequestMetrics<<L as MkStreamLabel>::StreamLabel>, S>;

// === impl RequestMetrics ===

impl<L: StreamLabel> RequestMetrics<L> {
    pub fn register(reg: &mut Registry, histo: impl IntoIterator<Item = f64>) -> Self {
        let duration =
            DurationFamily::new_with_constructor(MkDurationHistogram(histo.into_iter().collect()));
        reg.register_with_unit(
            "request_duration",
            "The time between request initialization and response completion",
            Unit::Seconds,
            duration.clone(),
        );

        let statuses = Family::default();
        reg.register(
            "request_statuses",
            "Completed request-response streams",
            statuses.clone(),
        );

        Self { duration, statuses }
    }
}

#[cfg(feature = "test-util")]
impl<L: StreamLabel> RequestMetrics<L> {
    pub fn get_statuses(&self, labels: &L::StatusLabels) -> Counter {
        (*self.statuses.get_or_create(labels)).clone()
    }
}

impl<L: StreamLabel> Default for RequestMetrics<L> {
    fn default() -> Self {
        Self {
            duration: DurationFamily::new_with_constructor(MkDurationHistogram(Arc::new([]))),
            statuses: Default::default(),
        }
    }
}

impl<L: StreamLabel> Clone for RequestMetrics<L> {
    fn clone(&self) -> Self {
        Self {
            duration: self.duration.clone(),
            statuses: self.statuses.clone(),
        }
    }
}

impl<ReqB, L, S> svc::Service<http::Request<ReqB>> for RecordRequestDuration<L, S>
where
    L: MkStreamLabel,
    L::StreamLabel: StreamLabel,
    S: svc::Service<http::Request<ReqB>, Response = http::Response<BoxBody>, Error = Error>,
{
    type Response = http::Response<BoxBody>;
    type Error = S::Error;
    type Future = super::ResponseFuture<L::StreamLabel, S::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<ReqB>) -> Self::Future {
        let (parts, body) = req.into_parts();
        let state = self.labeler.mk_stream_labeler(&parts).map(|labeler| {
            let (tx, start) = oneshot::channel();
            tx.send(time::Instant::now()).unwrap();
            let RequestMetrics { statuses, duration } = self.metric.clone();
            super::ResponseState {
                labeler,
                start,
                duration,
                statuses,
            }
        });

        let req = http::Request::from_parts(parts, body);
        let inner = self.inner.call(req);
        super::ResponseFuture { state, inner }
    }
}
