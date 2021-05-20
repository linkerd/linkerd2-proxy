//! A middleware that boxes HTTP response bodies.

use crate::BoxBody;
use futures::{future, TryFutureExt};
use linkerd_error::Error;
use linkerd_stack::layer;
use std::task::{Context, Poll};

#[derive(Clone, Debug)]
pub struct BoxResponse<S>(S);

impl<S> BoxResponse<S> {
    pub fn layer() -> impl layer::Layer<S, Service = Self> + Clone + Copy {
        layer::mk(Self)
    }
}

impl<S, Req, B> tower::Service<Req> for BoxResponse<S>
where
    S: tower::Service<Req, Response = http::Response<B>>,
    B: http_body::Body<Data = bytes::Bytes> + Send + 'static,
    B::Error: Into<Error> + 'static,
{
    type Response = http::Response<BoxBody>;
    type Error = S::Error;
    type Future = future::MapOk<S::Future, fn(S::Response) -> Self::Response>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        self.0.call(req).map_ok(|rsp| rsp.map(BoxBody::new))
    }
}
