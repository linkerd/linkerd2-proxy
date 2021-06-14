//! A middleware that boxes HTTP request bodies.

use crate::BoxBody;
use linkerd_error::Error;
use linkerd_stack::layer;
use std::{
    marker::PhantomData,
    task::{Context, Poll},
};

#[derive(Debug)]
pub struct BoxRequest<S, B>(S, PhantomData<fn(B)>);

impl<S, B> BoxRequest<S, B> {
    pub fn layer() -> impl layer::Layer<S, Service = Self> + Clone + Copy {
        layer::mk(|inner| BoxRequest(inner, PhantomData))
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

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        self.0.call(req.map(BoxBody::new))
    }
}
