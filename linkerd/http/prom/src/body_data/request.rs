//! Tower middleware to instrument request bodies.

pub use super::metrics::{BodyDataMetrics, RequestBodyFamilies};

use http::{Request, Response};
use linkerd_error::Error;
use linkerd_http_box::BoxBody;
use linkerd_stack::{self as svc, layer::Layer, ExtractParam, NewService, Service};
use std::marker::PhantomData;

/// A [`NewService<T>`] that creates [`RecordBodyData`] services.
#[derive(Clone, Debug)]
pub struct NewRecordBodyData<N, X, ReqX> {
    /// The inner [`NewService<T>`].
    inner: N,
    extract: X,
    marker: PhantomData<ReqX>,
}

/// Tracks body frames for an inner `S`-typed [`Service`].
#[derive(Clone, Debug)]
pub struct RecordBodyData<S, ReqX> {
    /// The inner [`Service<T>`].
    inner: S,
    extract: ReqX,
}

// === impl NewRecordBodyData ===

impl<N, X, ReqX> NewRecordBodyData<N, X, ReqX>
where
    X: Clone,
{
    /// Returns a [`Layer<S>`] that tracks body chunks.
    ///
    /// This uses an `X`-typed [`ExtractParam<P, T>`] implementation to extract service parameters
    /// from a `T`-typed target.
    pub fn new(extract: X) -> impl Layer<N, Service = Self> {
        svc::layer::mk(move |inner| Self {
            inner,
            extract: extract.clone(),
            marker: PhantomData,
        })
    }
}

impl<T, N, X, ReqX> NewService<T> for NewRecordBodyData<N, X, ReqX>
where
    N: NewService<T>,
    X: ExtractParam<ReqX, T>,
{
    type Service = RecordBodyData<N::Service, ReqX>;

    fn new_service(&self, target: T) -> Self::Service {
        let Self {
            inner,
            extract,
            marker: _,
        } = self;

        let extract = extract.extract_param(&target);
        let inner = inner.new_service(target);

        RecordBodyData { inner, extract }
    }
}

// === impl RecordBodyData ===

impl<ReqB, RespB, S, ReqX> Service<Request<ReqB>> for RecordBodyData<S, ReqX>
where
    S: Service<Request<BoxBody>, Response = Response<RespB>>,
    S::Future: Send + 'static,
    ReqB: http_body::Body + Send + 'static,
    ReqB::Data: Send + 'static,
    ReqB::Error: Into<Error>,
    ReqX: ExtractParam<BodyDataMetrics, Request<ReqB>>,
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
        let Self { inner, extract } = self;

        let req = {
            let metrics = extract.extract_param(&req);
            let instrument = |b| super::body::Body::new(b, metrics);
            req.map(instrument).map(BoxBody::new)
        };

        inner.call(req)
    }
}
