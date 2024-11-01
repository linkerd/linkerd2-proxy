use http_body::{Body, Frame};
use linkerd_error::Error;
use linkerd_http_box::BoxBody;
use linkerd_metrics::prom::Counter;
use linkerd_stack as svc;
use prometheus_client::{
    encoding::EncodeLabelSet,
    metrics::{
        family::{Family, MetricConstructor},
        histogram::Histogram,
    },
};
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{sync::oneshot, time};

mod request;
mod response;

pub use self::{
    request::{NewRequestDuration, RecordRequestDuration, RequestMetrics},
    response::{NewResponseDuration, RecordResponseDuration, ResponseMetrics},
};

/// A strategy for labeling request/responses streams for status and duration
/// metrics.
///
/// This is specifically to support higher-cardinality status counters and
/// lower-cardinality stream duration histograms.
pub trait MkStreamLabel {
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

    type StreamLabel: StreamLabel<
        DurationLabels = Self::DurationLabels,
        StatusLabels = Self::StatusLabels,
    >;

    /// Returns None when the request should not be recorded.
    fn mk_stream_labeler<B>(&self, req: &http::Request<B>) -> Option<Self::StreamLabel>;
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

    fn init_response<B>(&mut self, rsp: &http::Response<B>);
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

#[pin_project::pin_project]
pub struct ResponseFuture<L, F>
where
    L: StreamLabel,
{
    #[pin]
    inner: F,
    state: Option<ResponseState<L>>,
}

/// Notifies the response labeler when the response body is flushed.
#[pin_project::pin_project(PinnedDrop)]
struct ResponseBody<L: StreamLabel> {
    #[pin]
    inner: BoxBody,
    state: Option<ResponseState<L>>,
}

struct ResponseState<L: StreamLabel> {
    labeler: L,
    statuses: Family<L::StatusLabels, Counter>,
    duration: DurationFamily<L::DurationLabels>,
    start: oneshot::Receiver<time::Instant>,
}

type DurationFamily<L> = Family<L, Histogram, MkDurationHistogram>;

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
        RecordResponse::new(labeler, metric, inner)
    }
}

// === impl RecordResponse ===

impl<L, M, S> RecordResponse<L, M, S>
where
    L: MkStreamLabel,
{
    pub(crate) fn new(labeler: L, metric: M, inner: S) -> Self {
        Self {
            inner,
            labeler,
            metric,
        }
    }
}

// === impl ResponseFuture ===

impl<L, F> Future for ResponseFuture<L, F>
where
    L: StreamLabel,
    F: Future<Output = Result<http::Response<BoxBody>, Error>>,
{
    type Output = Result<http::Response<BoxBody>, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let res = futures::ready!(this.inner.poll(cx)).map_err(Into::into);
        let mut state = this.state.take();
        match res {
            Ok(rsp) => {
                if let Some(ResponseState { labeler, .. }) = state.as_mut() {
                    labeler.init_response(&rsp);
                }

                let (head, inner) = rsp.into_parts();
                if inner.is_end_stream() {
                    end_stream(&mut state, Ok(None));
                }
                Poll::Ready(Ok(http::Response::from_parts(
                    head,
                    BoxBody::new(ResponseBody { inner, state }),
                )))
            }
            Err(error) => {
                end_stream(&mut state, Err(&error));
                Poll::Ready(Err(error))
            }
        }
    }
}

// === impl ResponseBody ===

impl<L> http_body::Body for ResponseBody<L>
where
    L: StreamLabel,
{
    type Data = <BoxBody as http_body::Body>::Data;
    type Error = Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Error>>> {
        let this = self.project();
        let mut inner = this.inner;
        let state = this.state;

        // Poll the inner body for the next frame, check if the stream is now finished.
        let frame =
            futures::ready!(inner.as_mut().poll_frame(cx)).map(|res| res.map_err(Into::into));
        let eos = inner.is_end_stream();

        // Update telemetry if the end of the stream has been reached:
        match frame.as_ref() {
            // 1. The stream is over if a `None` has been yielded, or if an error occurred.
            None => end_stream(state, Ok(None)),
            Some(Err(error)) => end_stream(state, Err(error)),
            // 2. If we have a frame, check if it this is a trailers frame, or if the stream
            //    is now marked as complete.
            Some(Ok(frame)) => {
                if let trailers @ Some(_) = frame.trailers_ref() {
                    end_stream(state, Ok(trailers))
                } else if eos {
                    end_stream(state, Ok(None))
                }
            }
        }

        Poll::Ready(frame)
    }

    #[inline]
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}

#[pin_project::pinned_drop]
impl<L> PinnedDrop for ResponseBody<L>
where
    L: StreamLabel,
{
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        if this.state.is_some() {
            end_stream(this.state, Err(&RequestCancelled(()).into()));
        }
    }
}

fn end_stream<L>(
    state: &mut Option<ResponseState<L>>,
    res: Result<Option<&http::HeaderMap>, &Error>,
) where
    L: StreamLabel,
{
    let Some(ResponseState {
        duration,
        statuses: total,
        mut start,
        mut labeler,
    }) = state.take()
    else {
        return;
    };

    labeler.end_response(res);

    total.get_or_create(&labeler.status_labels()).inc();

    let elapsed = if let Ok(start) = start.try_recv() {
        time::Instant::now().saturating_duration_since(start)
    } else {
        time::Duration::ZERO
    };
    duration
        .get_or_create(&labeler.duration_labels())
        .observe(elapsed.as_secs_f64());
}
