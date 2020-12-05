//! A middleware that boxes HTTP request bodies.

use crate::BoxBody;
use linkerd2_error::Error;
use std::task::{Context, Poll};

#[derive(Debug)]
pub struct Layer<B>(std::marker::PhantomData<fn(B)>);

#[derive(Debug)]
pub struct BoxRequest<S, B>(S, std::marker::PhantomData<fn(B)>);

impl<B> Layer<B>
where
    B: http_body::Body + 'static,
{
    pub fn new() -> Self {
        Layer(std::marker::PhantomData)
    }
}

impl<B> Clone for Layer<B> {
    fn clone(&self) -> Self {
        Layer(self.0)
    }
}

impl<S, B> tower::layer::Layer<S> for Layer<B>
where
    B: http_body::Body + 'static,
    B::Data: Send + 'static,
    B::Error: Into<Error>,
    S: tower::Service<http::Request<BoxBody>>,
    BoxRequest<S, B>: tower::Service<http::Request<B>>,
{
    type Service = BoxRequest<S, B>;

    fn layer(&self, inner: S) -> Self::Service {
        BoxRequest(inner, self.0)
    }
}

impl<S: Clone, B> Clone for BoxRequest<S, B> {
    fn clone(&self) -> Self {
        BoxRequest(self.0.clone(), self.1)
    }
}

impl<S, B> tower::Service<http::Request<B>> for BoxRequest<S, B>
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
