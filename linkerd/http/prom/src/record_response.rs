use crate::stream_label::{LabelSet, MkStreamLabel, StreamLabel};
use http_body::Body;
use linkerd_error::Error;
use linkerd_http_body_eos::EosRef;
use linkerd_http_box::BoxBody;
use linkerd_stack as svc;
use prometheus_client::metrics::{
    family::{Family, MetricConstructor},
    histogram::Histogram,
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

/// A set of parameters that can be used to construct a `RecordResponse` layer.
pub struct Params<L: MkStreamLabel, M> {
    pub labeler: L,
    pub metric: M,
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("request was cancelled before completion")]
pub struct RequestCancelled;

/// Instruments an `N`-typed [`svc::NewService<T>`] with metrics.
///
/// Builds [`RecordResponse<L, M, S>`] instances by extracting `L`- and `M`-typed [`Params<L, M>`]
/// parameters from `T`-typed stack targets using an `X`-typed [`svc::ExtractParam<P, T>`]
/// implementation.
///
/// The `L`-typed [`MkStreamLabel`] inspects requests and emits a [`StreamLabel`], which is
/// intended to be used to generate labels for the `M`-typed metrics.
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
struct ResponseBody<L>
where
    L: StreamLabel,
    L::DurationLabels: LabelSet,
{
    #[pin]
    inner: BoxBody,
    state: Option<ResponseState<L>>,
}

struct ResponseState<L: StreamLabel> {
    labeler: L,
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

impl<L, X, M, N> NewRecordResponse<L, X, M, N>
where
    L: MkStreamLabel,
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

impl<L, M, N> NewRecordResponse<L, (), M, N>
where
    L: MkStreamLabel,
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
    L::DurationLabels: LabelSet,
    F: Future<Output = Result<http::Response<BoxBody>, Error>>,
{
    type Output = Result<http::Response<BoxBody>, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let res = futures::ready!(this.inner.poll(cx));
        let mut state = this.state.take();
        match res {
            Ok(rsp) => {
                if let Some(ResponseState { labeler, .. }) = state.as_mut() {
                    labeler.init_response(&rsp);
                }

                let (head, inner) = rsp.into_parts();
                if inner.is_end_stream() {
                    end_stream(&mut state, EosRef::None);
                }
                Poll::Ready(Ok(http::Response::from_parts(
                    head,
                    BoxBody::new(ResponseBody { inner, state }),
                )))
            }
            Err(error) => {
                end_stream(&mut state, EosRef::Error(&error));
                Poll::Ready(Err(error))
            }
        }
    }
}

// === impl ResponseBody ===

impl<L> http_body::Body for ResponseBody<L>
where
    L: StreamLabel,
    L::DurationLabels: LabelSet,
{
    type Data = <BoxBody as http_body::Body>::Data;
    type Error = Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let mut this = self.project();

        // Poll the inner body for the next frame.
        let poll = this.inner.as_mut().poll_frame(cx);
        let frame = futures::ready!(poll);

        match &frame {
            Some(Ok(frame)) => {
                if let Some(trls) = frame.trailers_ref() {
                    end_stream(this.state, EosRef::Trailers(trls));
                } else if this.inner.is_end_stream() {
                    end_stream(this.state, EosRef::None);
                }
            }
            Some(Err(error)) => end_stream(this.state, EosRef::Error(error)),
            None => end_stream(this.state, EosRef::None),
        }

        Poll::Ready(frame)
    }

    fn is_end_stream(&self) -> bool {
        // If the inner response state is still in place, the end of the stream has not been
        // classified and recorded yet.
        self.state.is_none()
    }
}

#[pin_project::pinned_drop]
impl<L> PinnedDrop for ResponseBody<L>
where
    L: StreamLabel,
    L::DurationLabels: LabelSet,
{
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        if this.state.is_some() {
            end_stream(this.state, EosRef::Cancelled);
        }
    }
}

fn end_stream<L>(state: &mut Option<ResponseState<L>>, res: EosRef<'_>)
where
    L: StreamLabel,
    L::DurationLabels: LabelSet,
{
    let Some(ResponseState {
        duration,
        mut start,
        mut labeler,
    }) = state.take()
    else {
        return;
    };

    labeler.end_response(res);

    let elapsed = if let Ok(start) = start.try_recv() {
        time::Instant::now().saturating_duration_since(start)
    } else {
        time::Duration::ZERO
    };
    duration
        .get_or_create(&labeler.duration_labels())
        .observe(elapsed.as_secs_f64());
}
