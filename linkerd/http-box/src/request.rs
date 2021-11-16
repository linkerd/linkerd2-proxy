//! A middleware that boxes HTTP request bodies.

use crate::{erase_request::EraseRequest, BoxBody};
use linkerd_error::Error;
use linkerd_stack::{layer, Proxy, Service};
use std::{
    marker::PhantomData,
    task::{Context, Poll},
};

#[derive(Debug)]
pub struct BoxRequest<B, S>(S, PhantomData<fn(B)>);

impl<B, S> BoxRequest<B, S> {
    pub fn layer() -> impl layer::Layer<S, Service = Self> + Clone + Copy {
        layer::mk(|inner| BoxRequest(inner, PhantomData))
    }
}

impl<S> BoxRequest<S, ()> {
    /// Constructs a boxing layer that erases the inner request type with [`EraseRequest`].
    pub fn erased() -> impl layer::Layer<S, Service = EraseRequest<S>> + Clone + Copy {
        EraseRequest::layer()
    }
}

impl<B, S: Clone> Clone for BoxRequest<B, S> {
    fn clone(&self) -> Self {
        BoxRequest(self.0.clone(), self.1)
    }
}

impl<B, S> Service<http::Request<B>> for BoxRequest<B, S>
where
    B: http_body::Body + Send + 'static,
    B::Data: Send + 'static,
    B::Error: Into<Error>,
    S: Service<http::Request<BoxBody>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        self.0.call(req.map(BoxBody::new))
    }
}

impl<B, S, P> Proxy<http::Request<B>, S> for BoxRequest<B, P>
where
    B: http_body::Body + Send + 'static,
    B::Data: Send + 'static,
    B::Error: Into<Error>,
    S: Service<P::Request>,
    P: Proxy<http::Request<BoxBody>, S>,
{
    type Request = P::Request;
    type Response = P::Response;
    type Error = P::Error;
    type Future = P::Future;

    #[inline]
    fn proxy(&self, inner: &mut S, req: http::Request<B>) -> Self::Future {
        self.0.proxy(inner, req.map(BoxBody::new))
    }
}
