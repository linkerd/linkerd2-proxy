use futures::{future, Future, Poll};
use linkerd2_error::Error;

pub trait Proxy<Req, S: tower::Service<Self::Request>> {
    type Request;
    type Response;
    type Error: Into<Error>;
    type Future: Future<Item = Self::Response, Error = Self::Error>;

    fn proxy(&self, inner: &mut S, req: Req) -> Self::Future;

    fn into_service(self, inner: S) -> Service<Self, S>
    where
        Self: Sized,
        S: tower::Service<Self::Request>,
    {
        Service::new(self, inner)
    }
}

#[derive(Clone, Debug)]
pub struct Service<P, S> {
    proxy: P,
    inner: S,
}

impl<Req, S> Proxy<Req, S> for ()
where
    S: tower::Service<Req>,
    S::Error: Into<Error>,
{
    type Request = Req;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn proxy(&self, inner: &mut S, req: Req) -> Self::Future {
        inner.call(req)
    }
}

impl<P, S> Service<P, S> {
    pub fn new(proxy: P, inner: S) -> Self {
        Self { proxy, inner }
    }

    pub fn into_parts(self) -> (P, S) {
        (self.proxy, self.inner)
    }
}

impl<Req, P, S> tower::Service<Req> for Service<P, S>
where
    P: Proxy<Req, S>,
    S: tower::Service<P::Request>,
    S::Error: Into<Error>,
{
    type Response = P::Response;
    type Error = Error;
    type Future = future::MapErr<P::Future, fn(P::Error) -> Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        self.proxy.proxy(&mut self.inner, req).map_err(Into::into)
    }
}
