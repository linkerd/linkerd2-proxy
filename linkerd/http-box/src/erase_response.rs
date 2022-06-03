//! A middleware that boxes HTTP response bodies.

use crate::BoxBody;
use futures::{future, TryFutureExt};
use linkerd_error::Error;
use linkerd_stack::{layer, Proxy, Service};
use std::task::{Context, Poll};

/// Boxes response bodies, erasing the original type.
///
/// This is *very* similar to the [`BoxResponse`](crate::response::BoxResponse)
/// middleware. However, that middleware is generic over a specific body type
/// that is erased. A given instance of `BoxResponse` can only erase the type
/// of one particular `Body` type, while this middleware will erase bodies of
/// *any* type.
///
/// An astute reader may ask, why not simply replace `BoxResponse` with this
/// middleware, if it is a more  flexible superset of the same behavior? The
/// answer is that in many cases, the use of this more flexible middleware
/// renders request body types uninferrable. If all `BoxResponse`s in the stack
/// are replaced with `EraseResponse`, suddenly a great deal of
/// `check_new_service` and `check_service` checks will require explicit
/// annotations for the pre-erasure body type. This is not great.
///
/// Instead, this type is implemented separately and should be used only when a
/// stack must be able to implement `Service<http::Response<B>>` for *multiple
/// distinct values of `B`*.
#[derive(Debug)]
pub struct EraseResponse<S>(S);

impl<S> EraseResponse<S> {
    pub fn new(inner: S) -> Self {
        Self(inner)
    }

    pub fn layer() -> impl layer::Layer<S, Service = Self> + Clone + Copy {
        layer::mk(Self::new)
    }
}

impl<S: Clone> Clone for EraseResponse<S> {
    fn clone(&self) -> Self {
        EraseResponse(self.0.clone())
    }
}

impl<S, R, B> Service<R> for EraseResponse<S>
where
    B: http_body::Body + Send + 'static,
    B::Data: Send + 'static,
    B::Error: Into<Error>,
    S: Service<R, Response = http::Response<B>>,
{
    type Response = http::Response<BoxBody>;
    type Error = S::Error;
    type Future = future::MapOk<S::Future, fn(S::Response) -> Self::Response>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, req: R) -> Self::Future {
        self.0.call(req).map_ok(|rsp| rsp.map(BoxBody::new))
    }
}

impl<S, R, B, P> Proxy<R, S> for EraseResponse<P>
where
    B: http_body::Body + Send + 'static,
    B::Data: Send + 'static,
    B::Error: Into<Error>,
    S: Service<P::Request>,
    P: Proxy<R, S, Response = http::Response<B>>,
{
    type Request = P::Request;
    type Response = http::Response<BoxBody>;
    type Error = P::Error;
    type Future = future::MapOk<P::Future, fn(P::Response) -> Self::Response>;

    #[inline]
    fn proxy(&self, inner: &mut S, req: R) -> Self::Future {
        self.0.proxy(inner, req).map_ok(|rsp| rsp.map(BoxBody::new))
    }
}
