use bytes::Buf;
use futures::{Async, Future, Poll};
use http;
use hyper::body::Payload;
use proxy;
use svc;

use std::{marker::PhantomData, mem};

#[derive(Debug)]
pub enum Error<A> {
    Fallback(http::Request<A>),
    Error(proxy::Error),
}

pub type Layer<P, F, A> = MakeFallback<svc::ServiceBuilder<P>, svc::ServiceBuilder<F>, A>;

#[derive(Debug)]
pub struct MakeFallback<P, F, A> {
    primary: P,
    fallback: F,
    _p: PhantomData<fn(A)>,
}

pub struct Service<P, F, A> {
    primary: P,
    fallback: F,
    _p: PhantomData<fn(A)>,
}

pub struct MakeFuture<P, F, A>
where
    P: Future,
    F: Future,
{
    primary: Making<P>,
    fallback: Making<F>,
    _p: PhantomData<fn(A)>,
}

pub struct ResponseFuture<P, F, A>
where
    P: Future<Error = Error<A>>,
    F: svc::Service<http::Request<A>>,
{
    fallback: F,
    state: State<P, F::Future>,
}

pub enum Body<A, B> {
    A(A),
    B(B),
}

enum State<P, F> {
    Primary(P),
    Fallback(F),
}

enum Making<T: Future> {
    NotReady(T),
    Ready(T::Item),
    Done,
}

pub fn layer<P, F, A>(
    primary: svc::ServiceBuilder<P>,
    fallback: svc::ServiceBuilder<F>,
) -> Layer<P, F, A> {
    Layer {
        primary,
        fallback,
        _p: PhantomData,
    }
}

impl<P, F, A, M> svc::Layer<M> for Layer<P, F, A>
where
    P: svc::Layer<M> + Clone,
    F: svc::Layer<M> + Clone,
    M: Clone,
{
    type Service = MakeFallback<P::Service, F::Service, A>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeFallback {
            primary: self.primary.clone().service(inner.clone()),
            fallback: self.fallback.clone().service(inner),
            _p: PhantomData,
        }
    }
}

impl<P, F, A, T> svc::Service<T> for MakeFallback<P, F, A>
where
    P: svc::Service<T>,
    P::Error: Into<proxy::Error>,
    F: svc::Service<T>,
    F::Error: Into<proxy::Error>,
    T: Clone,
{
    type Response = Service<P::Response, F::Response, A>;
    type Error = proxy::Error;
    type Future = MakeFuture<P::Future, F::Future, A>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        let primary_ready = self.primary.poll_ready().map_err(Into::into);
        let fallback_ready = self.fallback.poll_ready().map_err(Into::into);
        try_ready!(primary_ready);
        try_ready!(fallback_ready);
        Ok(Async::Ready(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        MakeFuture {
            primary: Making::NotReady(self.primary.call(target.clone())),
            fallback: Making::NotReady(self.fallback.call(target.clone())),
            _p: PhantomData,
        }
    }
}

impl<P, F, A> Clone for MakeFallback<P, F, A>
where
    P: Clone,
    F: Clone,
{
    fn clone(&self) -> Self {
        Self {
            primary: self.primary.clone(),
            fallback: self.fallback.clone(),
            _p: PhantomData,
        }
    }
}

impl<P, F, A> Future for MakeFuture<P, F, A>
where
    P: Future,
    P::Error: Into<proxy::Error>,
    F: Future,
    F::Error: Into<proxy::Error>,
{
    type Item = Service<P::Item, F::Item, A>;
    type Error = proxy::Error;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let done =
            self.primary.poll().map_err(Into::into)? && self.fallback.poll().map_err(Into::into)?;
        if !done {
            return Ok(Async::NotReady);
        }

        Ok(Async::Ready(Service {
            primary: self.primary.take(),
            fallback: self.fallback.take(),
            _p: PhantomData,
        }))
    }
}

impl<T: Future> Making<T> {
    fn poll(&mut self) -> Result<bool, T::Error> {
        let res = match *self {
            Making::NotReady(ref mut fut) => fut.poll()?,
            Making::Ready(_) => return Ok(true),
            Making::Done => panic!("polled after ready"),
        };
        match res {
            Async::Ready(res) => {
                *self = Making::Ready(res);
                Ok(true)
            }
            Async::NotReady => Ok(false),
        }
    }

    fn take(&mut self) -> T::Item {
        match mem::replace(self, Making::Done) {
            Making::Ready(a) => a,
            _ => panic!(),
        }
    }
}

impl<P, F, A, B, C> svc::Service<http::Request<A>> for Service<P, F, A>
where
    P: svc::Service<http::Request<A>, Response = http::Response<B>, Error = Error<A>>,
    F: svc::Service<http::Request<A>, Response = http::Response<C>> + Clone,
    F::Error: Into<proxy::Error>,
{
    type Response = http::Response<Body<B, C>>;
    type Error = proxy::Error;
    type Future = ResponseFuture<P::Future, F, A>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        let primary_ready = self.primary.poll_ready().map_err(|e| match e {
            Error::Fallback(_) => {
                panic!("service must not return a fallback request in poll_ready");
            }
            Error::Error(e) => e,
        });
        let fallback_ready = self.fallback.poll_ready().map_err(Into::into);
        try_ready!(primary_ready);
        try_ready!(fallback_ready);
        Ok(Async::Ready(()))
    }

    fn call(&mut self, req: http::Request<A>) -> Self::Future {
        ResponseFuture {
            fallback: self.fallback.clone(),
            state: State::Primary(self.primary.call(req)),
        }
    }
}

impl<P, F, A, B, C> Future for ResponseFuture<P, F, A>
where
    P: Future<Item = http::Response<B>, Error = Error<A>>,
    F: svc::Service<http::Request<A>, Response = http::Response<C>>,
    F::Error: Into<proxy::Error>,
{
    type Item = http::Response<Body<B, C>>;
    type Error = proxy::Error;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            self.state = match self.state {
                State::Primary(ref mut f) => match f.poll() {
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Ok(Async::Ready(rsp)) => return Ok(Async::Ready(rsp.map(Body::A))),
                    Err(Error::Error(e)) => return Err(e),
                    Err(Error::Fallback(req)) => State::Fallback(self.fallback.call(req)),
                },
                State::Fallback(ref mut f) => {
                    return f
                        .poll()
                        .map(|p| p.map(|rsp| rsp.map(Body::B)))
                        .map_err(Into::into)
                }
            }
        }
    }
}

impl<A, B> Payload for Body<A, B>
where
    A: Payload,
    B: Payload<Error = A::Error>,
{
    type Data = Body<A::Data, B::Data>;
    type Error = A::Error;

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, Self::Error> {
        match self {
            Body::A(ref mut body) => body.poll_data().map(|r| r.map(|o| o.map(Body::A))),
            Body::B(ref mut body) => body.poll_data().map(|r| r.map(|o| o.map(Body::B))),
        }
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, Self::Error> {
        match self {
            Body::A(ref mut body) => body.poll_trailers(),
            Body::B(ref mut body) => body.poll_trailers(),
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            Body::A(ref body) => body.is_end_stream(),
            Body::B(ref body) => body.is_end_stream(),
        }
    }
}

impl<A, B: Default> Default for Body<A, B> {
    fn default() -> Self {
        Body::B(Default::default())
    }
}

impl<A, B> Buf for Body<A, B>
where
    A: Buf,
    B: Buf,
{
    fn remaining(&self) -> usize {
        match self {
            Body::A(ref buf) => buf.remaining(),
            Body::B(ref buf) => buf.remaining(),
        }
    }

    fn bytes(&self) -> &[u8] {
        match self {
            Body::A(ref buf) => buf.bytes(),
            Body::B(ref buf) => buf.bytes(),
        }
    }

    fn advance(&mut self, cnt: usize) {
        match self {
            Body::A(ref mut buf) => buf.advance(cnt),
            Body::B(ref mut buf) => buf.advance(cnt),
        }
    }
}
