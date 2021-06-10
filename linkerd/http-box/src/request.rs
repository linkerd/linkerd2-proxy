//! A middleware that boxes HTTP request bodies.

use crate::BoxBody;
use linkerd_error::Error;
use linkerd_stack::{layer, Proxy};
use std::task::{Context, Poll};

#[derive(Debug)]
pub struct BoxRequest<S>(S);

impl<S> BoxRequest<S> {
    pub fn layer() -> impl layer::Layer<S, Service = Self> + Clone + Copy {
        layer::mk(BoxRequest)
    }
}

impl<S: Clone> Clone for BoxRequest<S> {
    fn clone(&self) -> Self {
        BoxRequest(self.0.clone())
    }
}

impl<S, B> tower::Service<http::Request<B>> for BoxRequest<S>
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

impl<S, B, P> Proxy<http::Request<B>, S> for BoxRequest<P>
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
