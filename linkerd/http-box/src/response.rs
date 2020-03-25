//! A middleware that boxes HTTP response bodies.

use crate::Payload;
use futures::{future, Future, Poll};
use linkerd2_error::Error;

#[derive(Copy, Clone, Debug)]
pub struct Layer(());

#[derive(Clone, Debug)]
pub struct BoxResponse<S>(S);

impl Layer {
    pub fn new() -> Self {
        Layer(())
    }
}

impl<S> tower::layer::Layer<S> for Layer {
    type Service = BoxResponse<S>;

    fn layer(&self, inner: S) -> Self::Service {
        BoxResponse(inner)
    }
}

impl<S, Req, B> tower::Service<Req> for BoxResponse<S>
where
    S: tower::Service<Req, Response = http::Response<B>>,
    B: hyper::body::Payload + Send + 'static,
    B::Error: Into<Error> + 'static,
{
    type Response = http::Response<Payload>;
    type Error = S::Error;
    type Future = future::Map<S::Future, fn(S::Response) -> Self::Response>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.0.poll_ready()
    }

    fn call(&mut self, req: Req) -> Self::Future {
        self.0.call(req).map(|rsp| rsp.map(Payload::new))
    }
}
