use linkerd_error::Error;
use linkerd_http_box::BoxBody;
use linkerd_metrics::prom::Counter;
use linkerd_stack as svc;
use prometheus_client::{
    encoding::EncodeLabelSet,
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
pub struct RequestMetrics<DurL, StatL> {
    duration: DurationFamily<DurL>,
    statuses: Family<StatL, Counter>,
}

pub type NewRequestDuration<L, X, N, ReqB, RespB> = super::NewRecordResponse<
    L,
    X,
    RequestMetrics<
        <<L as MkStreamLabel<ReqB, RespB>>::StreamLabel as StreamLabel<RespB>>::DurationLabels,
        <<L as MkStreamLabel<ReqB, RespB>>::StreamLabel as StreamLabel<RespB>>::StatusLabels,
    >,
    N,
    ReqB,
    RespB,
>;

pub type RecordRequestDuration<L, S, ReqB, RespB> = super::RecordResponse<
    L,
    RequestMetrics<
        <<L as MkStreamLabel<ReqB, RespB>>::StreamLabel as StreamLabel<RespB>>::DurationLabels,
        <<L as MkStreamLabel<ReqB, RespB>>::StreamLabel as StreamLabel<RespB>>::StatusLabels,
    >,
    S,
>;

// === impl RequestMetrics ===

impl<DurL, StatL> RequestMetrics<DurL, StatL>
where
    DurL: EncodeLabelSet + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
    StatL: EncodeLabelSet + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
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
impl<DurL, StatL> RequestMetrics<DurL, StatL>
where
    StatL: EncodeLabelSet + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
    DurL: EncodeLabelSet + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
{
    pub fn get_statuses(&self, labels: &StatL) -> Counter {
        (*self.statuses.get_or_create(labels)).clone()
    }
}

impl<DurL, StatL> Default for RequestMetrics<DurL, StatL>
where
    StatL: EncodeLabelSet + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
    DurL: EncodeLabelSet + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
{
    fn default() -> Self {
        Self {
            duration: DurationFamily::new_with_constructor(MkDurationHistogram(Arc::new([]))),
            statuses: Default::default(),
        }
    }
}

impl<DurL, StatL> Clone for RequestMetrics<DurL, StatL> {
    fn clone(&self) -> Self {
        Self {
            duration: self.duration.clone(),
            statuses: self.statuses.clone(),
        }
    }
}

impl<ReqB, L, S> svc::Service<http::Request<ReqB>> for RecordRequestDuration<L, S, ReqB, BoxBody>
where
    L: MkStreamLabel<ReqB, BoxBody>,
    L::StreamLabel: StreamLabel<BoxBody>,
    S: svc::Service<http::Request<ReqB>, Response = http::Response<BoxBody>, Error = Error>,
{
    type Response = http::Response<BoxBody>;
    type Error = S::Error;
    type Future = super::ResponseFuture<L::StreamLabel, S::Future, BoxBody>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<ReqB>) -> Self::Future {
        let state = self.labeler.mk_stream_labeler(&req).map(|labeler| {
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

        let inner = self.inner.call(req);
        super::ResponseFuture { state, inner }
    }
}
