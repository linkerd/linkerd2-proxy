use bytes::Buf;
use futures::{Future, Poll};
use http;
use hyper::body::Payload;
use proxy;
use svc;

/// A fallback layer composing two service builders.
///
/// If the future returned by the primary builder's `MakeService` fails with
/// an error matching a given predicate, the fallback future will attempt
/// to call the secondary `MakeService`.
#[derive(Debug)]
pub struct Layer<A, B, P = fn(&proxy::Error) -> bool> {
    primary: svc::Builder<A>,
    fallback: svc::Builder<B>,
    predicate: P,
}

#[derive(Debug, Clone)]
pub struct MakeSvc<A, B, P> {
    primary: A,
    fallback: B,
    predicate: P,
}

pub struct MakeFuture<A, B, P, T>
where
    A: Future,
    A::Error: Into<proxy::Error>,
    B: svc::Service<T>,
{
    fallback: B,
    target: Option<T>,
    predicate: P,
    state: FallbackState<A, B::Future, T>,
}

#[derive(Clone)]
pub enum Either<A, B> {
    A(A),
    B(B),
}

enum FallbackState<A, B, T> {
    /// Waiting for the primary service's future to complete.
    Primary(A),
    ///W aiting for the fallback service to become ready.
    Waiting(Option<T>),
    /// Waiting for the fallback service's future to complete.
    Fallback(B),
}

pub fn layer<A, B>(primary: svc::Builder<A>, fallback: svc::Builder<B>) -> Layer<A, B> {
    let predicate: fn(&proxy::Error) -> bool = |_| true;
    Layer {
        primary,
        fallback,
        predicate,
    }
}

// === impl Layer ===

impl<A, B> Layer<A, B> {
    /// Returns a `Layer` that uses the given `predicate` to determine whether
    /// to fall back.
    pub fn with_predicate<P>(self, predicate: P) -> Layer<A, B, P>
    where
        P: Fn(&proxy::Error) -> bool + Clone,
    {
        Layer {
            primary: self.primary,
            fallback: self.fallback,
            predicate,
        }
    }

    /// Returns a `Layer` that falls back if the error or its source is of
    /// type `E`.
    pub fn on_error<E>(self) -> Layer<A, B>
    where
        E: std::error::Error + 'static,
    {
        self.with_predicate(|e| e.is::<E>() || e.source().map(|s| s.is::<E>()).unwrap_or(false))
    }
}

impl<A, B, P, M> svc::Layer<M> for Layer<A, B, P>
where
    A: svc::Layer<M> + Clone,
    B: svc::Layer<M> + Clone,
    M: Clone,
    P: Fn(&proxy::Error) -> bool + Clone,
{
    type Service = MakeSvc<A::Service, B::Service, P>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeSvc {
            primary: self.primary.clone().service(inner.clone()),
            fallback: self.fallback.clone().service(inner),
            predicate: self.predicate.clone(),
            // _p: PhantomData,
        }
    }
}

impl<A, B, P> Clone for Layer<A, B, P>
where
    A: Clone,
    B: Clone,
    P: Clone,
{
    fn clone(&self) -> Self {
        Layer {
            primary: self.primary.clone(),
            fallback: self.fallback.clone(),
            predicate: self.predicate.clone(),
            // _p: PhantomData,
        }
    }
}

// === impl MakeSvc ===

impl<A, B, P, T> svc::Service<T> for MakeSvc<A, B, P>
where
    A: svc::Service<T>,
    A::Error: Into<proxy::Error>,
    B: svc::Service<T> + Clone,
    B::Error: Into<proxy::Error>,
    P: Fn(&proxy::Error) -> bool + Clone,
    T: Clone,
{
    type Response = Either<A::Response, B::Response>;
    type Error = proxy::Error;
    type Future = MakeFuture<A::Future, B, P, T>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.primary.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, target: T) -> Self::Future {
        MakeFuture {
            fallback: self.fallback.clone(),
            predicate: self.predicate.clone(),
            target: Some(target.clone()),
            state: FallbackState::Primary(self.primary.call(target)),
        }
    }
}

impl<A, B, P, T> Future for MakeFuture<A, B, P, T>
where
    A: Future,
    A::Error: Into<proxy::Error>,
    B: svc::Service<T>,
    B::Error: Into<proxy::Error>,
    P: Fn(&proxy::Error) -> bool,
{
    type Item = Either<A::Item, B::Response>;
    type Error = proxy::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            self.state = match self.state {
                // We've called the primary service and are waiting for its
                // future to complete.
                FallbackState::Primary(ref mut f) => match f.poll() {
                    Ok(r) => return Ok(r.map(Either::A)),
                    Err(error) => {
                        let error = error.into();
                        if (self.predicate)(&error) {
                            trace!("{} matches; trying to fall back", error);
                            FallbackState::Waiting(self.target.take())
                        } else {
                            trace!("{} does not match; not falling back", error);
                            return Err(error);
                        }
                    }
                },
                // The primary service has returned a fallback error, so we are
                // waiting for the fallback service to be ready.
                FallbackState::Waiting(ref mut target) => {
                    try_ready!(self.fallback.poll_ready().map_err(Into::into));
                    let target = target.take().expect("target should only be taken once");
                    FallbackState::Fallback(self.fallback.call(target))
                }
                // We've called the fallback service and are waiting for its
                // future to complete.
                FallbackState::Fallback(ref mut f) => {
                    return f.poll().map(|a| a.map(Either::B)).map_err(Into::into)
                }
            }
        }
    }
}

// === impl Either ===

impl<A, B, B1, B2, R> svc::Service<R> for Either<A, B>
where
    A: svc::Service<R, Response = http::Response<B1>>,
    A::Error: Into<proxy::Error>,
    B: svc::Service<R, Response = http::Response<B2>>,
    B::Error: Into<proxy::Error>,
{
    type Response = http::Response<Either<B1, B2>>;
    type Future = Either<A::Future, B::Future>;
    type Error = proxy::Error;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        match self {
            Either::A(ref mut inner) => inner.poll_ready().map_err(Into::into),
            Either::B(ref mut inner) => inner.poll_ready().map_err(Into::into),
        }
    }

    fn call(&mut self, req: R) -> Self::Future {
        match self {
            Either::A(ref mut inner) => Either::A(inner.call(req)),
            Either::B(ref mut inner) => Either::B(inner.call(req)),
        }
    }
}

impl<A, B, B1, B2> Future for Either<A, B>
where
    A: Future<Item = http::Response<B1>>,
    A::Error: Into<proxy::Error>,
    B: Future<Item = http::Response<B2>>,
    B::Error: Into<proxy::Error>,
{
    type Item = http::Response<Either<B1, B2>>;
    type Error = proxy::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self {
            Either::A(ref mut inner) => inner
                .poll()
                .map_err(Into::into)
                .map(|ready| ready.map(|rsp| rsp.map(Either::A))),
            Either::B(ref mut inner) => inner
                .poll()
                .map_err(Into::into)
                .map(|ready| ready.map(|rsp| rsp.map(Either::B))),
        }
    }
}

impl<A, B> Payload for Either<A, B>
where
    A: Payload,
    A::Error: Into<proxy::Error>,
    B: Payload,
    B::Error: Into<proxy::Error>,
{
    type Data = Either<A::Data, B::Data>;
    type Error = proxy::Error;

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, Self::Error> {
        match self {
            Either::A(ref mut body) => body
                .poll_data()
                .map(|r| r.map(|o| o.map(Either::A)))
                .map_err(Into::into),
            Either::B(ref mut body) => body
                .poll_data()
                .map(|r| r.map(|o| o.map(Either::B)))
                .map_err(Into::into),
        }
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, Self::Error> {
        match self {
            Either::A(ref mut body) => body.poll_trailers().map_err(Into::into),
            Either::B(ref mut body) => body.poll_trailers().map_err(Into::into),
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            Either::A(ref body) => body.is_end_stream(),
            Either::B(ref body) => body.is_end_stream(),
        }
    }
}

impl<A, B: Default> Default for Either<A, B> {
    fn default() -> Self {
        Either::B(Default::default())
    }
}

impl<A, B> Buf for Either<A, B>
where
    A: Buf,
    B: Buf,
{
    fn remaining(&self) -> usize {
        match self {
            Either::A(ref buf) => buf.remaining(),
            Either::B(ref buf) => buf.remaining(),
        }
    }

    fn bytes(&self) -> &[u8] {
        match self {
            Either::A(ref buf) => buf.bytes(),
            Either::B(ref buf) => buf.bytes(),
        }
    }

    fn advance(&mut self, cnt: usize) {
        match self {
            Either::A(ref mut buf) => buf.advance(cnt),
            Either::B(ref mut buf) => buf.advance(cnt),
        }
    }
}
