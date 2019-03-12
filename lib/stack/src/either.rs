use futures::{future, Future, Poll};

use svc;

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Describes two alternate `Layer`s, `Stacks`s or `Service`s.
#[derive(Clone, Debug)]
pub enum Either<A, B> {
    A(A),
    B(B),
}

impl<T, U, A, B, N> super::Layer<T, U, N> for Either<A, B>
where
    A: super::Layer<T, U, N>,
    A::Error: Into<Error>,
    B: super::Layer<T, U, N>,
    B::Error: Into<Error>,
    N: super::Stack<U>,
{
    type Value = <Either<A::Stack, B::Stack> as super::Stack<T>>::Value;
    type Error = Error;
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
    N::Error: Into<Error>,
    M: super::Stack<T>,
    M::Error: Into<Error>,
{
    type Value = Either<N::Value, M::Value>;
    type Error = Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        match self {
            Either::A(ref a) => a.make(target).map(Either::A).map_err(Into::into),
            Either::B(ref b) => b.make(target).map(Either::B).map_err(Into::into),
        }
    }
}

impl<A, B, R> svc::Service<R> for Either<A, B>
where
    A: svc::Service<R>,
    B: svc::Service<R, Response = A::Response>,
    A::Error: Into<Error>,
    B::Error: Into<Error>,
{
    type Response = A::Response;
    type Error = Error;
    type Future = future::Either<
        future::MapErr<A::Future, fn(A::Error) -> Error>,
        future::MapErr<B::Future, fn(B::Error) -> Error>,
    >;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        match self {
            Either::A(ref mut a) => a.poll_ready().map_err(Into::into),
            Either::B(ref mut b) => b.poll_ready().map_err(Into::into),
        }
    }

    fn call(&mut self, req: R) -> Self::Future {
        match self {
            Either::A(ref mut a) => future::Either::A(a.call(req).map_err(Into::into)),
            Either::B(ref mut b) => future::Either::B(b.call(req).map_err(Into::into)),
        }
    }
}
