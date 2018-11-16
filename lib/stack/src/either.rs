use futures::{Future, Poll, future};
use std::{error, fmt};

use svc;

/// Describes two alternate `Layer`s, `Stacks`s or `Service`s.
#[derive(Clone, Debug)]
pub enum Either<A, B> {
    A(A),
    B(B),
}

impl<T, U, A, B, N> super::Layer<T, U, N> for Either<A, B>
where
    A: super::Layer<T, U, N>,
    B: super::Layer<T, U, N, Error = A::Error>,
    N: super::Stack<U>
{
    type Value = <Either<A::Stack, B::Stack> as super::Stack<T>>::Value;
    type Error = <Either<A::Stack, B::Stack> as super::Stack<T>>::Error;
    type Stack = Either<A::Stack, B::Stack>;

    fn bind(&self, next: N) -> Self::Stack {
        match self {
            Either::A(ref a) => Either::A(a.bind(next)),
            Either::B(ref b) => Either::B(b.bind(next)),
        }
    }
}

impl<T, N, M> super::Stack<T> for Either<N, M>
where
    N: super::Stack<T>,
    M: super::Stack<T, Error = N::Error>,
{
    type Value = Either<N::Value, M::Value>;
    type Error = Either<N::Error, M::Error>;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        match self {
            Either::A(ref a) => a.make(target).map(Either::A).map_err(Either::A),
            Either::B(ref b) => b.make(target).map(Either::B).map_err(Either::B),
        }
    }
}

impl<A, B, R> svc::Service<R> for Either<A, B>
where
    A: svc::Service<R>,
    B: svc::Service<R, Response = A::Response>,
{
    type Response = A::Response;
    type Error = Either<A::Error, B::Error>;
    type Future = future::Either<
        future::MapErr<A::Future, fn(A::Error) -> Either<A::Error, B::Error>>,
        future::MapErr<B::Future, fn(B::Error) -> Either<A::Error, B::Error>>,
    >;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        match self {
            Either::A(ref mut a) => a.poll_ready().map_err(Either::A),
            Either::B(ref mut b) => b.poll_ready().map_err(Either::B),
        }
    }

    fn call(&mut self, req: R) -> Self::Future {
        match self {
            Either::A(ref mut a) => future::Either::A(a.call(req).map_err(Either::A)),
            Either::B(ref mut b) => future::Either::B(b.call(req).map_err(Either::B)),
        }
    }
}

impl<A: fmt::Display, B: fmt::Display> fmt::Display for Either<A, B> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Either::A(a) => a.fmt(f),
            Either::B(b) => b.fmt(f),
        }
    }
}

impl<A: error::Error, B: error::Error> error::Error for Either<A, B> {
    fn cause(&self) -> Option<&error::Error> {
        match self {
            Either::A(a) => a.cause(),
            Either::B(b) => b.cause(),
        }
    }
}
