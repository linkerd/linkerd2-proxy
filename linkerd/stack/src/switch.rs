//! A middleware that switches between two underlying stacks, depending on the
//! target type.

use futures::{future, prelude::*};
use linkerd2_error::Error;
use std::task::{Context, Poll};
use tower::util::ServiceExt;

/// Determines whether the primary stack should be used.
pub trait Switch<T> {
    fn use_primary(&self, target: &T) -> bool;
}

/// Makes either the primary or fallback stack, as determined by an `S`-typed
/// `Switch`.
#[derive(Clone, Debug)]
pub struct MakeSwitch<S, P, F> {
    switch: S,
    primary: P,
    fallback: F,
}

impl<T, F: Fn(&T) -> bool> Switch<T> for F {
    fn use_primary(&self, target: &T) -> bool {
        (self)(target)
    }
}

impl<S, P, F> MakeSwitch<S, P, F> {
    pub fn new(switch: S, primary: P, fallback: F) -> Self {
        MakeSwitch {
            switch,
            primary,
            fallback,
        }
    }

    pub fn layer(switch: S, fallback: F) -> impl super::layer::Layer<P, Service = Self> + Clone
    where
        S: Clone,
        F: Clone,
    {
        super::layer::mk(move |primary| Self::new(switch.clone(), primary, fallback.clone()))
    }
}

impl<T, S, P, F> super::NewService<T> for MakeSwitch<S, P, F>
where
    S: Switch<T>,
    P: super::NewService<T>,
    F: super::NewService<T>,
{
    type Service = tower::util::Either<P::Service, F::Service>;

    fn new_service(&mut self, target: T) -> Self::Service {
        if self.switch.use_primary(&target) {
            tower::util::Either::A(self.primary.new_service(target))
        } else {
            tower::util::Either::B(self.fallback.new_service(target))
        }
    }
}

impl<T, S, P, F> tower::Service<T> for MakeSwitch<S, P, F>
where
    S: Switch<T>,
    P: tower::Service<T> + Clone,
    P::Error: Into<Error>,
    F: tower::Service<T> + Clone,
    F::Error: Into<Error>,
{
    type Response = tower::util::Either<P::Response, F::Response>;
    type Error = Error;
    type Future = future::Either<
        future::MapOk<
            future::ErrInto<tower::util::Oneshot<P, T>, Error>,
            fn(P::Response) -> tower::util::Either<P::Response, F::Response>,
        >,
        future::MapOk<
            future::ErrInto<tower::util::Oneshot<F, T>, Error>,
            fn(F::Response) -> tower::util::Either<P::Response, F::Response>,
        >,
    >;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        if self.switch.use_primary(&target) {
            future::Either::Left(
                self.primary
                    .clone()
                    .oneshot(target)
                    .err_into::<Error>()
                    .map_ok(tower::util::Either::A),
            )
        } else {
            future::Either::Right(
                self.fallback
                    .clone()
                    .oneshot(target)
                    .err_into::<Error>()
                    .map_ok(tower::util::Either::B),
            )
        }
    }
}
