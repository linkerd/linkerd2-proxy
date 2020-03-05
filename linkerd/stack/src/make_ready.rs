use futures::{try_ready, Future, Poll};
use linkerd2_error::Error;
use std::marker::PhantomData;
use tower::util::Ready;

#[derive(Debug)]
pub struct MakeReadyLayer<Req>(PhantomData<fn(Req)>);

#[derive(Debug)]
pub struct MakeReady<M, Req>(M, PhantomData<fn(Req)>);

#[derive(Debug)]
pub enum MakeReadyFuture<F, S, Req> {
    Making(F),
    Ready(Ready<S, Req>),
}

impl<Req> MakeReadyLayer<Req> {
    pub fn new() -> Self {
        MakeReadyLayer(PhantomData)
    }
}

impl<M, Req> tower::layer::Layer<M> for MakeReadyLayer<Req> {
    type Service = MakeReady<M, Req>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeReady(inner, self.0)
    }
}

impl<Req> Clone for MakeReadyLayer<Req> {
    fn clone(&self) -> Self {
        MakeReadyLayer(self.0)
    }
}

impl<T, M, S, Req> tower::Service<T> for MakeReady<M, Req>
where
    M: tower::Service<T, Response = S>,
    M::Error: Into<Error>,
    S: tower::Service<Req>,
    S::Error: Into<Error>,
{
    type Response = S;
    type Error = Error;
    type Future = MakeReadyFuture<M::Future, S, Req>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.0.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, t: T) -> Self::Future {
        MakeReadyFuture::Making(self.0.call(t))
    }
}

impl<M: Clone, Req> Clone for MakeReady<M, Req> {
    fn clone(&self) -> Self {
        MakeReady(self.0.clone(), self.1)
    }
}

impl<F, S, Req> Future for MakeReadyFuture<F, S, Req>
where
    F: Future<Item = S>,
    F::Error: Into<Error>,
    S: tower::Service<Req>,
    S::Error: Into<Error>,
{
    type Item = S;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            *self = match self {
                MakeReadyFuture::Making(ref mut fut) => {
                    let svc = try_ready!(fut.poll().map_err(Into::into));
                    MakeReadyFuture::Ready(Ready::new(svc))
                }
                MakeReadyFuture::Ready(ref mut fut) => {
                    return fut.poll().map_err(Into::into);
                }
            }
        }
    }
}
