#![deny(warnings, rust_2018_idioms)]

//! A middleware that fails requests by policy.

use futures::{future, TryFutureExt};
use linkerd2_error::Error;
use std::task::{Context, Poll};

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

impl<A, S> AdmitService<A, S> {
    pub fn new(admit: A, inner: S) -> Self {
        Self { admit, inner }
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
        future::Ready<Result<Self::Response, Self::Error>>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, t: T) -> Self::Future {
        match self.admit.admit(&t) {
            Ok(()) => future::Either::Left(self.inner.call(t).map_err(Into::into)),
            Err(e) => future::Either::Right(future::err(e.into())),
        }
    }
}
