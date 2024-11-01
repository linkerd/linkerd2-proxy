//! Tower middleware to instrument response bodies.

pub use super::metrics::{BodyDataMetrics, ResponseBodyFamilies};

use http::{Request, Response};
use http_body::Body;
use linkerd_error::Error;
use linkerd_http_box::BoxBody;
use linkerd_stack::{self as svc, layer::Layer, ExtractParam, NewService, Service};
use std::{future::Future, pin::Pin};

/// A [`NewService<T>`] that creates [`RecordBodyData`] services.
#[derive(Clone, Debug)]
pub struct NewRecordBodyData<X, N> {
    /// The [`ExtractParam<P, T>`] strategy for obtaining our parameters.
    extract: X,
    /// The inner [`NewService<T>`].
    inner: N,
}

/// Tracks body frames for an inner `S`-typed [`Service`].
#[derive(Clone, Debug)]
pub struct RecordBodyData<S> {
    /// The inner [`Service<T>`].
    inner: S,
    /// The metrics to be affixed to the response body.
    metrics: BodyDataMetrics,
}

// === impl NewRecordBodyData ===

impl<X: Clone, N> NewRecordBodyData<X, N> {
    /// Returns a [`Layer<S>`] that tracks body chunks.
    ///
    /// This uses an `X`-typed [`ExtractParam<P, T>`] implementation to extract service parameters
    /// from a `T`-typed target.
    pub fn layer_via(extract: X) -> impl Layer<N, Service = Self> + Clone {
        svc::layer::mk(move |inner| Self {
            extract: extract.clone(),
            inner,
        })
    }
}

impl<T, X, N> NewService<T> for NewRecordBodyData<X, N>
where
    X: ExtractParam<BodyDataMetrics, T>,
    N: NewService<T>,
{
    type Service = RecordBodyData<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let Self { extract, inner } = self;

        let metrics = extract.extract_param(&target);
        let inner = inner.new_service(target);

        RecordBodyData { inner, metrics }
    }
}

// === impl RecordBodyData ===

impl<ReqB, RespB, S> Service<Request<ReqB>> for RecordBodyData<S>
where
    S: Service<Request<ReqB>, Response = Response<RespB>>,
    S::Future: Send + 'static,
    RespB: Body + Send + 'static,
    RespB::Data: Send + 'static,
    RespB::Error: Into<Error>,
{
    type Response = Response<BoxBody>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    #[inline]
    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqB>) -> Self::Future {
        use futures::{FutureExt, TryFutureExt};

        let Self { inner, metrics } = self;
        let metrics = metrics.clone();
        let instrument = Box::new(|resp| Self::instrument_response(resp, metrics));

        inner.call(req).map_ok(instrument).boxed()
    }
}

impl<S> RecordBodyData<S> {
    fn instrument_response<B>(resp: Response<B>, metrics: BodyDataMetrics) -> Response<BoxBody>
    where
        B: Body + Send + 'static,
        B::Data: Send + 'static,
        B::Error: Into<Error>,
    {
        resp.map(|b| super::body::Body::new(b, metrics))
            .map(BoxBody::new)
    }
}
