//! A middleware that fails requests by policy.

use futures::{future, Future, Poll};
use linkerd2_error::Error;

pub struct AdmitLayer<A>(A);

pub trait Admit<T> {
    type Error: Into<Error>;

    fn admit(&mut self, target: &T) -> Result<(), Self::Error>;
}

#[derive(Debug, Clone)]
pub struct AdmitService<A, S> {
    admit: A,
    inner: S,
}

impl<A> AdmitLayer<A> {
    pub fn new(admit: A) -> Self {
        Self(admit)
    }
}

impl<A: Clone, S> tower::layer::Layer<S> for AdmitLayer<A> {
    type Service = AdmitService<A, S>;

    fn layer(&self, inner: S) -> Self::Service {
        Self::Service {
            admit: self.0.clone(),
            inner,
        }
    }
}

impl<A, T, S> tower::Service<T> for AdmitService<A, S>
where
    A: Admit<T>,
    S: tower::Service<T>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::Either<
        future::MapErr<S::Future, fn(S::Error) -> Error>,
        future::FutureResult<Self::Response, Self::Error>,
    >;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, t: T) -> Self::Future {
        match self.admit.admit(&t) {
            Ok(()) => future::Either::A(self.inner.call(t).map_err(Into::into)),
            Err(e) => future::Either::B(future::err(e.into())),
        }
    }
}
