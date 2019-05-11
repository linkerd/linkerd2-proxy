use bytes::Buf;
use futures::{Async, Future, Poll};
use http;
use hyper::body::Payload;
use proxy;
use svc;

use std::{fmt, marker::PhantomData, mem};

#[derive(Debug)]
pub struct Error<A> {
    error: proxy::Error,
    fallback: Option<http::Request<A>>,
}

#[derive(Debug)]
pub struct Layer<P, F, A> {
    primary: svc::Builder<P>,
    fallback: svc::Builder<F>,
    _p: PhantomData<fn(A)>,
}

#[derive(Debug)]
pub struct MakeSvc<P, F, A> {
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
    state: ResponseState<P, F::Future, A>,
}

pub enum Body<A, B> {
    A(A),
    B(B),
}

enum ResponseState<P, F, A> {
    /// Waiting for the primary service's future to complete.
    Primary(P),
    /// Request buffered, waiting for the fallback service to become ready.
    Waiting(Option<http::Request<A>>),
    /// Waiting for the fallback service's future to complete.
    Fallback(F),
}

enum Making<T: Future> {
    NotReady(T),
    Ready(T::Item),
    Done,
}

pub fn layer<P, F, A>(primary: svc::Builder<P>, fallback: svc::Builder<F>) -> Layer<P, F, A> {
    Layer {
        primary,
        fallback,
        _p: PhantomData,
    }
}

// === impl Layer ===

impl<P, F, A, M> svc::Layer<M> for Layer<P, F, A>
where
    P: svc::Layer<M> + Clone,
    F: svc::Layer<M> + Clone,
    M: Clone,
{
    type Service = MakeSvc<P::Service, F::Service, A>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeSvc {
            primary: self.primary.clone().service(inner.clone()),
            fallback: self.fallback.clone().service(inner),
            _p: PhantomData,
        }
    }
}

impl<P, F, A> Clone for Layer<P, F, A>
where
    P: Clone,
    F: Clone,
{
    fn clone(&self) -> Self {
        layer(self.primary.clone(), self.fallback.clone())
    }
}

// === impl MakeSvc ===

impl<P, F, A, T> svc::Service<T> for MakeSvc<P, F, A>
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
        try_ready!(fallback_ready);
        primary_ready
    }

    fn call(&mut self, target: T) -> Self::Future {
        MakeFuture {
            primary: Making::NotReady(self.primary.call(target.clone())),
            fallback: Making::NotReady(self.fallback.call(target.clone())),
            _p: PhantomData,
        }
    }
}

impl<P, F, A> Clone for MakeSvc<P, F, A>
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

// === impl MakeFuture ===

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
        // Are both the primary and fallback futures finished?
        try_ready!(self.primary.poll().map_err(Into::into));
        try_ready!(self.fallback.poll().map_err(Into::into));

        Ok(Async::Ready(Service {
            primary: self.primary.take(),
            fallback: self.fallback.take(),
            _p: PhantomData,
        }))
    }
}

impl<T: Future> Making<T> {
    fn poll(&mut self) -> Poll<(), T::Error> {
        *self = match self {
            Making::NotReady(ref mut fut) => {
                let res = try_ready!(fut.poll());
                Making::Ready(res)
            }
            Making::Ready(_) => return Ok(Async::Ready(())),
            Making::Done => panic!("polled after ready"),
        };
        Ok(Async::Ready(()))
    }

    fn take(&mut self) -> T::Item {
        match mem::replace(self, Making::Done) {
            Making::Ready(a) => a,
            _ => panic!("tried to take service twice"),
        }
    }
}

// === impl Service ===

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
        // We're ready as long as the primary service is ready. If we have to
        // call the fallback service, the response future will buffer the
        // request until the fallback service is ready.
        self.primary.poll_ready().map_err(|e| e.error)
    }

    fn call(&mut self, req: http::Request<A>) -> Self::Future {
        ResponseFuture {
            fallback: self.fallback.clone(),
            state: ResponseState::Primary(self.primary.call(req)),
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
        use self::ResponseState::*;
        loop {
            self.state = match self.state {
                // We've called the primary service and are waiting for its
                // future to complete.
                Primary(ref mut f) => match f.poll() {
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Ok(Async::Ready(rsp)) => return Ok(Async::Ready(rsp.map(Body::A))),
                    Err(Error {
                        fallback: Some(req),
                        error,
                    }) => {
                        trace!("{}; trying to fall back", error);
                        Waiting(Some(req))
                    }
                    Err(e) => return Err(e.into()),
                },
                // The primary service has returned a fallback error, so we are
                // waiting for the fallback service to be ready.
                Waiting(ref mut req) => {
                    try_ready!(self.fallback.poll_ready().map_err(Into::into));
                    let req = req.take().expect("request should only be taken once");
                    Fallback(self.fallback.call(req))
                }
                // We've called the fallback service and are waiting for its
                // future to complete.
                Fallback(ref mut f) => {
                    return f
                        .poll()
                        .map(|p| p.map(|rsp| rsp.map(Body::B)))
                        .map_err(Into::into);
                }
            }
        }
    }
}

// === impl Body ===

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

// === impl Error ===

impl<A> fmt::Display for Error<A> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(&self.error, f)
    }
}

impl<A> Into<proxy::Error> for Error<A> {
    fn into(self) -> proxy::Error {
        self.error
    }
}

impl<A> From<proxy::Error> for Error<A> {
    fn from(error: proxy::Error) -> Self {
        Error {
            error,
            fallback: None,
        }
    }
}

impl<A> Error<A> {
    pub fn fallback(req: http::Request<A>, error: impl Into<proxy::Error>) -> Self {
        Error {
            fallback: Some(req),
            error: error.into(),
        }
    }
}
