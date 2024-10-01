use linkerd_error::Error;
use linkerd_stack as svc;
use prometheus_client::{
    encoding::EncodeLabelSet,
    metrics::{
        family::{Family, MetricConstructor},
        histogram::Histogram,
    },
};
use std::sync::Arc;

mod request;
mod response;
mod response_future;

pub use self::{
    request::{NewRequestDuration, RecordRequestDuration, RequestMetrics},
    response::{NewResponseDuration, RecordResponseDuration, ResponseMetrics},
    response_future::{ResponseFuture, ResponseState},
};

/// A strategy for labeling request/responses streams for status and duration
/// metrics.
///
/// This is specifically to support higher-cardinality status counters and
/// lower-cardinality stream duration histograms.
pub trait MkStreamLabel {
    type StreamLabel: StreamLabel;

    /// Returns None when the request should not be recorded.
    fn mk_stream_labeler(&self, req: &http::request::Parts) -> Option<Self::StreamLabel>;
}

pub trait StreamLabel: Send + 'static {
    type DurationLabels: EncodeLabelSet
        + Clone
        + Eq
        + std::fmt::Debug
        + std::hash::Hash
        + Send
        + Sync
        + 'static;
    type StatusLabels: EncodeLabelSet
        + Clone
        + Eq
        + std::fmt::Debug
        + std::hash::Hash
        + Send
        + Sync
        + 'static;

    fn init_response(&mut self, rsp: &http::response::Parts);
    fn end_response(&mut self, trailers: Result<Option<&http::HeaderMap>, &Error>);

    fn status_labels(&self) -> Self::StatusLabels;
    fn duration_labels(&self) -> Self::DurationLabels;
}

/// A set of parameters that can be used to construct a `RecordResponse` layer.
pub struct Params<L: MkStreamLabel, M> {
    pub labeler: L,
    pub metric: M,
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("request was cancelled before completion")]
pub struct RequestCancelled(());

/// Builds RecordResponse instances by extracing M-typed parameters from stack
/// targets
#[derive(Clone, Debug)]
pub struct NewRecordResponse<L, X, M, N> {
    inner: N,
    extract: X,
    _marker: std::marker::PhantomData<fn() -> (L, M)>,
}

/// A Service that can record a request/response durations.
#[derive(Clone, Debug)]
pub struct RecordResponse<L, M, S> {
    inner: S,
    labeler: L,
    metric: M,
}

/// A family of labeled duration histograms.
type DurationFamily<L> = Family<L, Histogram, MkDurationHistogram>;

/// Creates new [`Histogram`]s.
///
/// See [`MkDurationHistogram::new_metric()`].
#[derive(Clone, Debug)]
struct MkDurationHistogram(Arc<[f64]>);

// === impl MkDurationHistogram ===

impl MetricConstructor<Histogram> for MkDurationHistogram {
    fn new_metric(&self) -> Histogram {
        Histogram::new(self.0.iter().copied())
    }
}

// === impl NewRecordResponse ===

impl<M, X, K, N> NewRecordResponse<M, X, K, N>
where
    M: MkStreamLabel,
{
    pub fn new(extract: X, inner: N) -> Self {
        Self {
            extract,
            inner,
            _marker: std::marker::PhantomData,
        }
    }

    pub fn layer_via(extract: X) -> impl svc::layer::Layer<N, Service = Self> + Clone
    where
        X: Clone,
    {
        svc::layer::mk(move |inner| Self::new(extract.clone(), inner))
    }
}

impl<M, K, N> NewRecordResponse<M, (), K, N>
where
    M: MkStreamLabel,
{
    pub fn layer() -> impl svc::layer::Layer<N, Service = Self> + Clone {
        Self::layer_via(())
    }
}

impl<T, L, X, M, N> svc::NewService<T> for NewRecordResponse<L, X, M, N>
where
    L: MkStreamLabel,
    X: svc::ExtractParam<Params<L, M>, T>,
    N: svc::NewService<T>,
{
    type Service = RecordResponse<L, M, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let Params { labeler, metric } = self.extract.extract_param(&target);
        let inner = self.inner.new_service(target);
        RecordResponse {
            labeler,
            metric,
            inner,
        }
    }
}
