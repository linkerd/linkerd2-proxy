use super::Resolve;
use crate::Error;
use futures::future::{self, Either, FutureResult, MapErr};
use futures::Future;

/// Determines whether a given target is resolveable.
pub trait Admit<T> {
    fn admit(&self, target: &T) -> bool;
}

/// Indicates that a target was not admitted.
#[derive(Copy, Clone, Debug)]
pub struct Rejected(());

/// Wraps an `R`-typed `Resolve`, rejecting requests that are not admitted.
pub struct Filter<A, R> {
    admit: A,
    resolve: R,
}

// === impl Filter ===

impl<A, R> Filter<A, R> {
    pub fn new<T>(admit: A, resolve: R) -> Self
    where
        Self: Resolve<T>,
    {
        Self { admit, resolve }
    }
}

impl<T, A, R> Resolve<T> for Filter<A, R>
where
    A: Admit<T>,
    R: Resolve<T>,
    <R::Future as Future>::Error: Into<Error>,
{
    type Endpoint = R::Endpoint;
    type Future = Either<
        MapErr<R::Future, fn(<R::Future as Future>::Error) -> Error>,
        FutureResult<R::Resolution, Error>,
    >;
    type Resolution = R::Resolution;

    fn resolve(&self, target: &T) -> Self::Future {
        if self.admit.admit(target) {
            Either::A(self.resolve.resolve(target).map_err(Into::into))
        } else {
            Either::B(future::err(Rejected(()).into()))
        }
    }
}

// === impl Rejected ===

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rejected")
    }
}

impl std::error::Error for Rejected {}
