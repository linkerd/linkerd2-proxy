use http_body::Body;
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
    _marker: std::marker::PhantomData<(L, M)>,
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

/// Inner state used by [`ResponseFuture`] and [`ResponseBody`].
///
/// This is used to update Prometheus metrics across the response's lifecycle.
///
/// This is generic across an `L`-typed [`StreamLabel`], which bears responsibility for labelling
/// responses according to their status code and duration.
struct ResponseState<L: StreamLabel> {
    labeler: L,
    /// The family of [`Counter`]s tracking response status code.
    statuses: Family<L::StatusLabels, Counter>,
    /// The family of [`Histogram`]s tracking response durations.
    duration: DurationFamily<L::DurationLabels>,
    /// Receives a timestamp noting when the service received a request.
    start: oneshot::Receiver<time::Instant>,
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

// === impl ResponseFuture ===

impl<L, F> Future for ResponseFuture<L, F>
where
    L: StreamLabel,
    F: Future<Output = Result<http::Response<BoxBody>, Error>>,
{
    /// A [`ResponseFuture`] produces the same response as its inner `F`-typed future.
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        // Poll the inner future, returning if it isn't ready yet.
        let res = futures::ready!(this.inner.poll(cx)).map_err(Into::into);

        // We got a response back! Take our state, and examine the output.
        let mut state = this.state.take();
        match res {
            Ok(rsp) => {
                let (head, inner) = rsp.into_parts();
                if let Some(ResponseState { labeler, .. }) = state.as_mut() {
                    labeler.init_response(&head);
                }

                // Call `end_stream` if the body is empty.
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

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Error>>> {
        let mut this = self.project();
        let res =
            futures::ready!(this.inner.as_mut().poll_data(cx)).map(|res| res.map_err(Into::into));
        if let Some(Err(error)) = res.as_ref() {
            end_stream(this.state, Err(error));
        } else if (*this.inner).is_end_stream() {
            end_stream(this.state, Ok(None));
        }
        Poll::Ready(res)
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Error>> {
        let this = self.project();
        let res = futures::ready!(this.inner.poll_trailers(cx)).map_err(Into::into);
        end_stream(this.state, res.as_ref().map(Option::as_ref));
        Poll::Ready(res)
    }

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
