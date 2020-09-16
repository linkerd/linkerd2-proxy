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
pub struct MakeSwitch<S, P, F> {
    switch: S,
    primary: P,
    fallback: F,
}

pub enum SwitchService<P, F> {
    Primary(P),
    Fallback(F),
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

impl<T, S, P, F> tower::Service<T> for MakeSwitch<S, P, F>
where
    T: Send + 'static,
    S: Switch<T>,
    P: tower::Service<T> + Clone + Send + 'static,
    P::Error: Into<Error>,
    P::Future: Send,
    F: tower::Service<T> + Clone + Send + 'static,
    F::Error: Into<Error>,
    F::Future: Send,
{
    type Response = SwitchService<P::Response, F::Response>;
    type Error = Error;
    type Future = future::Either<
        future::MapOk<
            future::ErrInto<tower::util::Oneshot<P, T>, Error>,
            fn(P::Response) -> SwitchService<P::Response, F::Response>,
        >,
        future::MapOk<
            future::ErrInto<tower::util::Oneshot<F, T>, Error>,
            fn(F::Response) -> SwitchService<P::Response, F::Response>,
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
                    .map_ok(SwitchService::Primary),
            )
        } else {
            future::Either::Right(
                self.fallback
                    .clone()
                    .oneshot(target)
                    .err_into::<Error>()
                    .map_ok(SwitchService::Fallback),
            )
        }
    }
}

impl<T, P, F> tower::Service<T> for SwitchService<P, F>
where
    P: tower::Service<T>,
    P::Error: Into<Error>,
    P::Future: Send + 'static,
    F: tower::Service<T, Response = P::Response>,
    F::Error: Into<Error>,
    F::Future: Send + 'static,
{
    type Response = P::Response;
    type Error = Error;
    type Future =
        future::Either<future::ErrInto<P::Future, Error>, future::ErrInto<F::Future, Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(match self {
            Self::Primary(d) => futures::ready!(d.poll_ready(cx)).map_err(Into::into),
            Self::Fallback(f) => futures::ready!(f.poll_ready(cx)).map_err(Into::into),
        })
    }

    fn call(&mut self, req: T) -> Self::Future {
        match self {
            Self::Primary(d) => future::Either::Left(d.call(req).err_into::<Error>()),
            Self::Fallback(f) => future::Either::Right(f.call(req).err_into::<Error>()),
        }
    }
}
