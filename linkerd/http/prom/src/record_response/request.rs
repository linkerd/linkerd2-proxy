use super::{DurationFamily, MkDurationHistogram};
use crate::stream_label::{LabelSet, MkStreamLabel};
use linkerd_error::Error;
use linkerd_http_box::BoxBody;
use linkerd_stack as svc;
use prometheus_client::registry::{Registry, Unit};
use std::{
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{sync::oneshot, time};

/// Metrics type that tracks completed requests.
#[derive(Debug)]
pub struct RequestMetrics<L> {
    duration: DurationFamily<L>,
}

pub type NewRequestDuration<L, X, N> =
    super::NewRecordResponse<L, X, RequestMetrics<<L as MkStreamLabel>::DurationLabels>, N>;

pub type RecordRequestDuration<L, S> =
    super::RecordResponse<L, RequestMetrics<<L as MkStreamLabel>::DurationLabels>, S>;

// === impl RequestMetrics ===

impl<L> RequestMetrics<L>
where
    L: LabelSet,
{
    pub fn register(reg: &mut Registry, histo: impl IntoIterator<Item = f64>) -> Self {
        let duration =
            DurationFamily::new_with_constructor(MkDurationHistogram(histo.into_iter().collect()));
        reg.register_with_unit(
            "request_duration",
            "The time between request initialization and response completion",
            Unit::Seconds,
            duration.clone(),
        );

        Self { duration }
    }
}

impl<L> Default for RequestMetrics<L>
where
    L: LabelSet,
{
    fn default() -> Self {
        Self {
            duration: DurationFamily::new_with_constructor(MkDurationHistogram(Arc::new([]))),
        }
    }
}

impl<L> Clone for RequestMetrics<L> {
    fn clone(&self) -> Self {
        Self {
            duration: self.duration.clone(),
        }
    }
}

impl<ReqB, L, S> svc::Service<http::Request<ReqB>> for RecordRequestDuration<L, S>
where
    L: MkStreamLabel,
    L::StatusLabels: LabelSet,
    L::DurationLabels: LabelSet,
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
        let state = self.labeler.mk_stream_labeler(&req).map(|labeler| {
            let (tx, start) = oneshot::channel();
            tx.send(time::Instant::now()).unwrap();
            let RequestMetrics { duration } = self.metric.clone();
            super::ResponseState {
                labeler,
                start,
                duration,
            }
        });

        let inner = self.inner.call(req);
        super::ResponseFuture { state, inner }
    }
}
