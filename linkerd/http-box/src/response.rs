//! A middleware that boxes HTTP response bodies.

use crate::BoxBody;
use futures::{future, TryFutureExt};
use linkerd_error::Error;
use linkerd_stack::{layer, Proxy, Service};
use std::task::{Context, Poll};

#[derive(Clone, Debug)]
pub struct BoxResponse<S>(S);

impl<S> BoxResponse<S> {
    pub fn layer() -> impl layer::Layer<S, Service = Self> + Clone + Copy {
        layer::mk(Self)
    }
}

impl<S, Req, B> Service<Req> for BoxResponse<S>
where
    S: Service<Req, Response = http::Response<B>>,
    B: http_body::Body + Send + 'static,
    B::Data: Send + 'static,
    B::Error: Into<Error> + 'static,
{
    type Response = http::Response<BoxBody>;
    type Error = S::Error;
    type Future = future::MapOk<S::Future, fn(S::Response) -> Self::Response>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, req: Req) -> Self::Future {
        self.0.call(req).map_ok(|rsp| rsp.map(BoxBody::new))
    }
}

impl<Req, B, S, P> Proxy<Req, S> for BoxResponse<P>
where
    B: http_body::Body + Send + 'static,
    B::Data: Send + 'static,
    B::Error: Into<Error>,
    S: Service<P::Request>,
    P: Proxy<Req, S, Response = http::Response<B>>,
{
    type Request = P::Request;
    type Response = http::Response<BoxBody>;
    type Error = P::Error;
    type Future = future::MapOk<P::Future, fn(P::Response) -> Self::Response>;

    #[inline]
    fn proxy(&self, inner: &mut S, req: Req) -> Self::Future {
        self.0.proxy(inner, req).map_ok(|rsp| rsp.map(BoxBody::new))
    }
}
