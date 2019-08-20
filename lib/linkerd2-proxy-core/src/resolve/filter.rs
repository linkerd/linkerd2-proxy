use super::Resolve;
use crate::Error;
use futures::future::{self, Either, FutureResult, MapErr};
use futures::{Future, Poll};
use tower::Service;

/// Determines whether a given target is resolveable.
pub trait Admit<T> {
    fn admit(&self, target: &T) -> bool;
}

/// Wraps an `R`-typed `Resolve`, rejecting requests that are not admitted.
pub struct Filter<T, A, R> {
    admit: A,
    resolve: R,
    mk_err: fn(&T) -> Error,
}

// === impl Filter ===

impl<T, A, R> Filter<T, A, R>
where
    Self: Resolve<T>,
{
    pub fn new(admit: A, resolve: R, mk_err: fn(&T) -> Error) -> Self {
        Self {
            admit,
            resolve,
            mk_err,
        }
    }
}

impl<T, A, R> Service<T> for Filter<T, A, R>
where
    A: Admit<T>,
    R: Resolve<T>,
    <R::Future as Future>::Error: Into<Error>,
{
    type Response = R::Resolution;
    type Error = Error;
    type Future = Either<
        MapErr<R::Future, fn(<R::Future as Future>::Error) -> Error>,
        FutureResult<R::Resolution, Error>,
    >;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.resolve.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, target: T) -> Self::Future {
        if self.admit.admit(&target) {
            let fut = self.resolve.resolve(target);
            Either::A(fut.map_err(Into::into))
        } else {
            let err = (self.mk_err)(&target);
            Either::B(future::err(err))
        }
    }
}
