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
    registry::{Registry, Unit},
};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{sync::oneshot, time};

pub trait MkStreamLabel {
    type TotalLabels: EncodeLabelSet
        + Clone
        + Eq
        + std::fmt::Debug
        + std::hash::Hash
        + Send
        + Sync
        + 'static;
    type DurationLabels: EncodeLabelSet
        + Clone
        + Eq
        + std::fmt::Debug
        + std::hash::Hash
        + Send
        + Sync
        + 'static;

    type StreamLabel: StreamLabel<
        TotalLabels = Self::TotalLabels,
        DurationLabels = Self::DurationLabels,
    >;

    /// Returns None when the request should not be recorded.
    fn mk_stream_labeler<B>(&self, req: &http::Request<B>) -> Option<Self::StreamLabel>;
}

pub trait StreamLabel: Send + 'static {
    type TotalLabels: EncodeLabelSet
        + Clone
        + Eq
        + std::fmt::Debug
        + std::hash::Hash
        + Send
        + Sync
        + 'static;
    type DurationLabels: EncodeLabelSet
        + Clone
        + Eq
        + std::fmt::Debug
        + std::hash::Hash
        + Send
        + Sync
        + 'static;

    fn init_response<B>(&mut self, rsp: &http::Response<B>);

    fn end_response(&mut self, trailers: Result<Option<&http::HeaderMap>, &Error>);

    fn duration_labels(&self) -> Self::DurationLabels;
    fn total_labels(&self) -> Self::TotalLabels;
}

pub struct Params<L: MkStreamLabel, M> {
    pub labeler: L,
    pub metric: M,
}

#[derive(Clone, Debug)]
pub struct RequestMetrics<DurL, TotL> {
    duration: DurationFamily<DurL>,
    total: Family<TotL, Counter>,
}

#[derive(Clone, Debug)]
pub struct ResponseMetrics<DurL, TotL> {
    duration: DurationFamily<DurL>,
    total: Family<TotL, Counter>,
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

pub type NewRequestDuration<L, X, N> = NewRecordResponse<
    L,
    X,
    RequestMetrics<<L as MkStreamLabel>::DurationLabels, <L as MkStreamLabel>::TotalLabels>,
    N,
>;

pub type RecordRequestDuration<L, S> = RecordResponse<
    L,
    RequestMetrics<<L as MkStreamLabel>::DurationLabels, <L as MkStreamLabel>::TotalLabels>,
    S,
>;

pub type NewResponseDuration<L, X, N> = NewRecordResponse<
    L,
    X,
    ResponseMetrics<<L as MkStreamLabel>::DurationLabels, <L as MkStreamLabel>::TotalLabels>,
    N,
>;

pub type RecordResponseDuration<L, S> = RecordResponse<
    L,
    ResponseMetrics<<L as MkStreamLabel>::DurationLabels, <L as MkStreamLabel>::TotalLabels>,
    S,
>;

#[pin_project::pin_project]
pub struct ResponseFuture<L, F>
where
    L: StreamLabel,
{
    #[pin]
    inner: F,
    state: Option<ResponseState<L>>,
}

/// Notifies the response body when the request body is flushed.
#[pin_project::pin_project(PinnedDrop)]
struct RequestBody<B> {
    #[pin]
    inner: B,
    flushed: Option<oneshot::Sender<time::Instant>>,
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
    total: Family<L::TotalLabels, Counter>,
    duration: DurationFamily<L::DurationLabels>,
    start: oneshot::Receiver<time::Instant>,
}

type DurationFamily<L> = Family<L, Histogram, MkDurationHistogram>;

#[derive(Clone, Debug, Default)]
struct MkDurationHistogram(());

// === impl RequestDuration ===

impl<DurL, TotL> RequestMetrics<DurL, TotL>
where
    DurL: EncodeLabelSet + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
    TotL: EncodeLabelSet + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
{
    pub fn register(reg: &mut Registry) -> Self {
        let duration = DurationFamily::new_with_constructor(MkDurationHistogram(()));
        reg.register_with_unit(
            "request_duration",
            "The time between request initialization and response completion",
            Unit::Seconds,
            duration.clone(),
        );

        let total = Family::default();
        reg.register("requests", "Completed requests", total.clone());

        Self { duration, total }
    }
}

impl<DurL, TotL> Default for RequestMetrics<DurL, TotL>
where
    TotL: EncodeLabelSet + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
    DurL: EncodeLabelSet + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
{
    fn default() -> Self {
        Self {
            duration: DurationFamily::new_with_constructor(MkDurationHistogram(())),
            total: Default::default(),
        }
    }
}

// === impl RequestDuration ===

impl<DurL, TotL> ResponseMetrics<DurL, TotL>
where
    DurL: EncodeLabelSet + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
    TotL: EncodeLabelSet + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
{
    pub fn register(reg: &mut Registry) -> Self {
        let duration = DurationFamily::new_with_constructor(MkDurationHistogram(()));
        reg.register_with_unit(
            "response_duration",
            "The time between request completion and response completion",
            Unit::Seconds,
            duration.clone(),
        );

        let total = Family::default();
        reg.register("responses", "Completed responses", total.clone());

        Self { duration, total }
    }
}

impl<DurL, TotL> Default for ResponseMetrics<DurL, TotL>
where
    TotL: EncodeLabelSet + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
    DurL: EncodeLabelSet + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
{
    fn default() -> Self {
        Self {
            duration: DurationFamily::new_with_constructor(MkDurationHistogram(())),
            total: Default::default(),
        }
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

impl<ReqB, L, S> svc::Service<http::Request<ReqB>> for RecordRequestDuration<L, S>
where
    L: MkStreamLabel,
    S: svc::Service<http::Request<ReqB>, Response = http::Response<BoxBody>, Error = Error>,
{
    type Response = http::Response<BoxBody>;
    type Error = S::Error;
    type Future = ResponseFuture<L::StreamLabel, S::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<ReqB>) -> Self::Future {
        let state = self.labeler.mk_stream_labeler(&req).map(|labeler| {
            let RequestMetrics { total, duration } = self.metric.clone();
            let (tx, start) = oneshot::channel();
            tx.send(time::Instant::now()).unwrap();
            ResponseState {
                labeler,
                start,
                duration,
                total,
            }
        });

        let inner = self.inner.call(req);
        ResponseFuture { state, inner }
    }
}

impl<M, S> svc::Service<http::Request<BoxBody>> for RecordResponseDuration<M, S>
where
    M: MkStreamLabel,
    S: svc::Service<http::Request<BoxBody>, Response = http::Response<BoxBody>, Error = Error>,
{
    type Response = http::Response<BoxBody>;
    type Error = Error;
    type Future = ResponseFuture<M::StreamLabel, S::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<BoxBody>) -> Self::Future {
        // If there's a labeler, wrap the request body to record the time that
        // the respond flushes.
        let state = if let Some(labeler) = self.labeler.mk_stream_labeler(&req) {
            let (tx, start) = oneshot::channel();
            req = req.map(|inner| {
                BoxBody::new(RequestBody {
                    inner,
                    flushed: Some(tx),
                })
            });
            let ResponseMetrics { duration, total } = self.metric.clone();
            Some(ResponseState {
                labeler,
                start,
                duration,
                total,
            })
        } else {
            None
        };

        let inner = self.inner.call(req);
        ResponseFuture { state, inner }
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

fn end_stream<L>(
    state: &mut Option<ResponseState<L>>,
    res: Result<Option<&http::HeaderMap>, &Error>,
) where
    L: StreamLabel,
{
    let Some(ResponseState {
        duration,
        total,
        mut start,
        mut labeler,
    }) = state.take()
    else {
        return;
    };

    labeler.end_response(res);

    total.get_or_create(&labeler.total_labels()).inc();

    let elapsed = if let Ok(start) = start.try_recv() {
        time::Instant::now().saturating_duration_since(start)
    } else {
        time::Duration::ZERO
    };
    duration
        .get_or_create(&labeler.duration_labels())
        .observe(elapsed.as_secs_f64());
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

// === impl MkDurationHistogram ===

impl MkDurationHistogram {
    const BUCKETS: &'static [f64] = &[0.025, 0.1, 0.25, 1.0, 2.5, 10.0, 25.0];
}

impl MetricConstructor<Histogram> for MkDurationHistogram {
    fn new_metric(&self) -> Histogram {
        Histogram::new(Self::BUCKETS.iter().copied())
    }
}
