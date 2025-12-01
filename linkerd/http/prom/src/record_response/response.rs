use super::{DurationFamily, MkDurationHistogram};
use crate::stream_label::{LabelSet, MkStreamLabel};
use linkerd_error::Error;
use linkerd_http_body_eos::{BodyWithEosFn, EosRef};
use linkerd_http_box::BoxBody;
use linkerd_stack as svc;
use prometheus_client::registry::{Registry, Unit};
use std::{
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{sync::oneshot, time};

#[derive(Debug)]
pub struct ResponseMetrics<L> {
    duration: DurationFamily<L>,
}

pub type NewResponseDuration<L, X, N> =
    super::NewRecordResponse<L, X, ResponseMetrics<<L as MkStreamLabel>::DurationLabels>, N>;

pub type RecordResponseDuration<L, S> =
    super::RecordResponse<L, ResponseMetrics<<L as MkStreamLabel>::DurationLabels>, S>;

// === impl ResponseMetrics ===

impl<L: LabelSet> ResponseMetrics<L> {
    pub fn register(reg: &mut Registry, histo: impl IntoIterator<Item = f64>) -> Self {
        let duration =
            DurationFamily::new_with_constructor(MkDurationHistogram(histo.into_iter().collect()));
        reg.register_with_unit(
            "response_duration",
            "The time between request completion and response completion",
            Unit::Seconds,
            duration.clone(),
        );

        Self { duration }
    }
}

impl<L: LabelSet> Default for ResponseMetrics<L> {
    fn default() -> Self {
        Self {
            duration: DurationFamily::new_with_constructor(MkDurationHistogram(Arc::new([]))),
        }
    }
}

impl<L> Clone for ResponseMetrics<L> {
    fn clone(&self) -> Self {
        Self {
            duration: self.duration.clone(),
        }
    }
}

impl<M, S> svc::Service<http::Request<BoxBody>> for RecordResponseDuration<M, S>
where
    M: MkStreamLabel,
    M::DurationLabels: LabelSet,
    S: svc::Service<http::Request<BoxBody>, Response = http::Response<BoxBody>, Error = Error>,
{
    type Response = http::Response<BoxBody>;
    type Error = Error;
    type Future = super::ResponseFuture<M::StreamLabel, S::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<BoxBody>) -> Self::Future {
        // If there's a labeler, wrap the request body to record the time that
        // the respond flushes.
        let state = if let Some(labeler) = self.labeler.mk_stream_labeler(&req) {
            let (tx, start) = oneshot::channel();
            let on_eos = move |_: EosRef<'_>| {
                tx.send(time::Instant::now()).ok();
            };
            req = req
                .map(|inner| BodyWithEosFn::new(inner, on_eos))
                .map(BoxBody::new);
            let ResponseMetrics { duration } = self.metric.clone();
            Some(super::ResponseState {
                labeler,
                start,
                duration,
            })
        } else {
            None
        };

        let inner = self.inner.call(req);
        super::ResponseFuture { state, inner }
    }
}
