#![deny(warnings, rust_2018_idioms)]

//! A middleware that fails requests by policy.

use futures::{future, TryFutureExt};
use linkerd2_error::Error;
use linkerd2_stack::{NewService, ResultService};
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

impl<A, T, S> NewService<T> for AdmitService<A, S>
where
    A: Admit<T>,
    S: NewService<T>,
{
    type Service = ResultService<S::Service, A::Error>;

    fn new_service(&mut self, t: T) -> Self::Service {
        match self.admit.admit(&t) {
            Ok(()) => ResultService::ok(self.inner.new_service(t)),
            Err(e) => ResultService::err(e),
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
