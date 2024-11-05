//! A Tower middleware for counting response status codes.

use http::{response, HeaderMap, Response, StatusCode};
use http_body::SizeHint;
use linkerd_error::Error;
use linkerd_metrics::prom::{
    self,
    encoding::{EncodeLabelSet, LabelSetEncoder},
    EncodeLabelSetMut,
};
use linkerd_stack::{self as svc, NewService, Service};
use pin_project::pin_project;
use prometheus_client::encoding::{text::encode, EncodeLabelValue};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

pub use self::metrics::ResponseStatusFamilies;

mod metrics;

/// A [`NewService<T>`] that creates [`RecordResponseStatus`] services.
#[derive(Clone, Debug)]
pub struct NewRecordResponseStatus<N, L> {
    /// The inner [`NewService<T>`].
    inner: N,
    /// The metric families that will be updated.
    metrics: ResponseStatusFamilies<L>,
}

/// Counts the response status code for an inner `S`-typed [`Service`].
#[derive(Clone, Debug)]
pub struct RecordResponseStatus<S, L> {
    /// The inner [`Service<T>`].
    inner: S,
    /// The metrics to be affixed to the response body.
    metrics: ResponseStatusFamilies<L>,
}

/*
/// Observes a response, and determines its status code.
trait LabelStatus {
    /// The type of value used to represent the status code.
    type Label;
    /// Examines the response headers.
    fn response_start(&mut self, parts: response::Parts) -> Self::Label;
    /// Examines the response trailers (if any) or any errors that occured while polling the body.
    fn response_end(
        self,
        trailers: Result<Option<&http::HeaderMap>, &Error>,
    ) -> Option<Self::Label>;
}
*/

/// Counts this response after polling the inner body to completion.
#[pin_project]
pub struct StatusCountBody<B, L> {
    #[pin]
    inner: B,
    status: StatusCode,
    metrics: ResponseStatusFamilies<L>,
}

// === impl NewRecordResponseStatus ===

impl<N, L: Clone> NewRecordResponseStatus<N, L> {
    /// Returns a [`Layer<S>`][svc::layer::Layer] that counts requests.
    ///
    /// This uses an `X`-typed [`ExtractParam<P, T>`][svc::ExtractParam] implementation to extract
    /// [`RequestCount`] from a `T`-typed target.
    pub fn layer_via(
        metrics: ResponseStatusFamilies<L>,
    ) -> impl svc::layer::Layer<N, Service = Self> + Clone {
        svc::layer::mk(move |inner| Self {
            inner,
            metrics: metrics.clone(),
        })
    }
}

/// A [`NewRecordResponseStatus`] creates new [`RecordResponseStatus`] instances.
impl<T, N, L> NewService<T> for NewRecordResponseStatus<N, L>
where
    N: svc::NewService<T>,
    L: Clone,
{
    type Service = RecordResponseStatus<N::Service, L>;
    fn new_service(&self, target: T) -> Self::Service {
        let Self { inner, metrics } = self;
        let inner = inner.new_service(target);
        Self::Service {
            inner,
            metrics: metrics.clone(),
        }
    }
}

// === impl RecordResponseStatus ===

/// Records the response status of the inner service.
impl<T, RespB, S, L> Service<T> for RecordResponseStatus<S, L>
where
    S: Service<T, Response = Response<RespB>>,
    S::Future: Send + 'static,
    L: Clone + Send + Sync + 'static,
{
    type Response = Response<StatusCountBody<RespB, L>>;
    type Error = S::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    #[inline]
    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: T) -> Self::Future {
        let Self { inner, metrics } = self;

        let fut = inner.call(req);
        let metrics = metrics.clone();
        Box::pin(async move {
            let resp = fut.await?;
            let (parts, body) = resp.into_parts();
            // XXX
            let body = StatusCountBody {
                inner: body,
                status: parts.status,
                metrics: metrics.clone(),
            };
            // XXX
            let resp = Response::from_parts(parts, body);
            Ok(resp)
        })
    }
}

// === impl StatusCountBody ===

/// A [`StatusCountBody`] wraps its inner body.
impl<B, L> http_body::Body for StatusCountBody<B, L>
where
    B: http_body::Body,
{
    type Error = B::Error;
    type Data = B::Data;

    /// Attempt to pull out the next data buffer of this stream.
    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let this = self.project();
        this.inner.poll_data(cx)
    }

    /// Poll for an optional **single** `HeaderMap` of trailers.
    ///
    /// This function should only be called once `poll_data` returns `None`.
    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<HeaderMap>, Self::Error>> {
        // XXX(kate); examine the trailers here.
        let this = self.project();
        this.inner.poll_trailers(cx)
    }

    #[inline]
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    #[inline]
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

// impl labels

struct LabelsWithStatus<L, S, E> {
    /// The other labels to which
    labels: L,
    /// The value of the status code label.
    status: Option<S>,
    /// Any errors that may have occured.
    error: Option<E>,
}

impl<L, S, E> EncodeLabelSet for LabelsWithStatus<L, S, E>
where
    L: EncodeLabelSetMut,
    for<'a> &'a S: EncodeLabelValue,
    for<'a> &'a E: EncodeLabelValue,
{
    /// Encode oneself into the given encoder.
    fn encode(&self, mut encoder: LabelSetEncoder<'_>) -> Result<(), std::fmt::Error> {
        use prom::encoding::EncodeLabel;

        let Self {
            labels,
            status,
            error,
        } = self;

        ("http_status", status.as_ref()).encode(encoder.encode_label())?;
        ("error", error.as_ref()).encode(encoder.encode_label())?;

        labels.encode(encoder)?;

        Ok(())
    }
}
