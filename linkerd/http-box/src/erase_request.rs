//! A middleware that boxes HTTP request bodies.
use linkerd_error::Error;
use linkerd_http_box::BoxBody;
use linkerd_stack::{layer, Proxy};
use std::task::{Context, Poll};

/// A middleware that boxes HTTP request bodies.
///
/// This is *very* similar to the [`BoxRequest`] middleware. However, that
/// middleware is generic over a specific body type that is erased. A given
/// instance of `EraseRequest` can only erase the type of one particular `Body`
/// type, while this middleware will erase
/// bodies of *any* type.
///
/// An astute reader may ask, why not simply replace `BoxRequest` with this
/// middleware, if it is a more  flexible superset of the same behavior? The
/// answer is that in many cases, the use of this more flexible middleware
/// renders request body types uninferrable. If all `BoxRequest`s in the stack
/// are replaced with `EraseRequest`, suddenly a great deal of
/// `check_new_service` and `check_service` checks will require explicit
/// annotations for the pre-erasure body type. This is not great.
///
/// Instead, this type is implemented separately and should be used only when a
/// stack must be able to implement `Service<http::Request<B>>` for *multiple
/// distinct values of `B`*.
#[derive(Debug)]
pub struct EraseRequest<S>(S);

impl<S> EraseRequest<S> {
    pub fn layer() -> impl layer::Layer<S, Service = Self> + Clone + Copy {
        layer::mk(EraseRequest)
    }
}

impl<S: Clone> Clone for EraseRequest<S> {
    fn clone(&self) -> Self {
        EraseRequest(self.0.clone())
    }
}

impl<S, B> tower::Service<http::Request<B>> for EraseRequest<S>
where
    B: http_body::Body + Send + 'static,
    B::Data: Send + 'static,
    B::Error: Into<Error>,
    S: tower::Service<http::Request<BoxBody>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        self.0.call(req.map(BoxBody::new))
    }
}

impl<S, B, P> Proxy<http::Request<B>, S> for EraseRequest<P>
where
    B: http_body::Body + Send + 'static,
    B::Data: Send + 'static,
    B::Error: Into<Error>,
    S: tower::Service<P::Request>,
    P: Proxy<http::Request<BoxBody>, S>,
{
    type Request = P::Request;
    type Response = P::Response;
    type Error = P::Error;
    type Future = P::Future;

    fn proxy(&self, inner: &mut S, req: http::Request<B>) -> Self::Future {
        self.0.proxy(inner, req.map(BoxBody::new))
    }
}
