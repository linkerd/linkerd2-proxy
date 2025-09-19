//! Tower middleware to instrument request bodies.

pub use super::metrics::{BodyDataMetrics, RequestBodyFamilies};

use http::{Request, Response};
use linkerd_error::Error;
use linkerd_http_box::BoxBody;
use linkerd_stack::{self as svc, layer::Layer, ExtractParam, NewService, Param, Service};
use std::marker::PhantomData;

/// A [`NewService<T>`] that creates [`RecordBodyData`] services.
#[derive(Clone, Debug)]
pub struct NewRecordBodyData<N, ReqX, L> {
    /// The inner [`NewService<T>`].
    inner: N,
    metrics: RequestBodyFamilies<L>,
    marker: PhantomData<ReqX>,
}

/// Tracks body frames for an inner `S`-typed [`Service`].
#[derive(Clone, Debug)]
pub struct RecordBodyData<S, ReqX, L> {
    /// The inner [`Service<T>`].
    inner: S,
    extract: ReqX,
    metrics: RequestBodyFamilies<L>,
}

// === impl NewRecordBodyData ===

impl<N, ReqX, L> NewRecordBodyData<N, ReqX, L>
where
    L: Clone,
{
    /// Returns a [`Layer<S>`] that tracks body chunks.
    ///
    /// This uses an `X`-typed [`ExtractParam<P, T>`] implementation to extract service parameters
    /// from a `T`-typed target.
    pub fn new(metrics: RequestBodyFamilies<L>) -> impl Layer<N, Service = Self> {
        svc::layer::mk(move |inner| Self {
            inner,
            metrics: metrics.clone(),
            marker: PhantomData,
        })
    }
}

impl<T, N, ReqX, L> NewService<T> for NewRecordBodyData<N, ReqX, L>
where
    N: NewService<T>,
    T: Param<ReqX>,
    L: Clone,
{
    type Service = RecordBodyData<N::Service, ReqX, L>;

    fn new_service(&self, target: T) -> Self::Service {
        let Self {
            inner,
            metrics,
            marker: _,
        } = self;

        let extract = target.param();
        let inner = inner.new_service(target);
        let metrics = metrics.clone();

        RecordBodyData {
            inner,
            extract,
            metrics,
        }
    }
}

// === impl RecordBodyData ===

impl<ReqB, RespB, S, ReqX, L> Service<Request<ReqB>> for RecordBodyData<S, ReqX, L>
where
    S: Service<Request<BoxBody>, Response = Response<RespB>>,
    S::Future: Send + 'static,
    ReqB: http_body::Body + Send + 'static,
    ReqB::Data: Send + 'static,
    ReqB::Error: Into<Error>,
    ReqX: ExtractParam<L, Request<ReqB>>,
    L: linkerd_metrics::prom::encoding::EncodeLabelSet
        + std::fmt::Debug
        + std::hash::Hash
        + Eq
        + Clone
        + Send
        + Sync
        + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqB>) -> Self::Future {
        let Self {
            inner,
            extract,
            metrics,
        } = self;

        let req = {
            let labels = extract.extract_param(&req);
            let metrics = metrics.metrics(&labels);
            let instrument = |b| super::body::Body::new(b, metrics);
            req.map(instrument).map(BoxBody::new)
        };

        inner.call(req)
    }
}
