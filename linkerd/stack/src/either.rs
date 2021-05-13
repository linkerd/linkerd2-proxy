use crate::{layer, NewService, Service};
use futures::TryFutureExt;
use linkerd_error::Error;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

#[derive(Clone, Debug)]
pub struct NewEither<L, R> {
    left: L,
    right: R,
}

/// Combine two different service types into a single type.
///
/// Both services must be of the same request, response, and error types.
/// [`Either`] is useful for handling conditional branching in service middleware
/// to different inner service types.
///
/// *Unlike* Tower's `Either` type, the future returned by the `Service` impl
/// for `Either` is boxed, rather than an `Either` future.
#[derive(Clone, Debug)]
pub enum Either<A, B> {
    /// One type of backing [`Service`].
    A(A),
    /// The other type of backing [`Service`].
    B(B),
}

impl<A, B, Request> Service<Request> for Either<A, B>
where
    A: Service<Request>,
    A::Error: Into<Error> + 'static,
    A::Future: Send + 'static,
    B: Service<Request, Response = A::Response>,
    B::Error: Into<Error> + 'static,
    B::Future: Send + 'static,
{
    type Response = A::Response;
    type Error = Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        use self::Either::*;

        match self {
            A(service) => service.poll_ready(cx).map(|res| res.map_err(Into::into)),
            B(service) => service.poll_ready(cx).map(|res| res.map_err(Into::into)),
        }
    }

    fn call(&mut self, request: Request) -> Self::Future {
        use self::Either::*;

        match self {
            A(service) => Box::pin(service.call(request).map_err(Into::into)),
            B(service) => Box::pin(service.call(request).map_err(Into::into)),
        }
    }
}

impl<S, A, B> layer::Layer<S> for Either<A, B>
where
    A: layer::Layer<S>,
    B: layer::Layer<S>,
{
    type Service = Either<A::Service, B::Service>;

    fn layer(&self, inner: S) -> Self::Service {
        match self {
            Either::A(layer) => Either::A(layer.layer(inner)),
            Either::B(layer) => Either::B(layer.layer(inner)),
        }
    }
}

// === impl NewEither ===

impl<L, R> NewEither<L, R> {
    pub fn new(left: L, right: R) -> Self {
        Self { left, right }
    }
}

impl<L, R: Clone> NewEither<L, R> {
    pub fn layer(right: R) -> impl layer::Layer<L, Service = Self> + Clone {
        layer::mk(move |left| Self::new(left, right.clone()))
    }
}

impl<T, U, L, R> NewService<Either<T, U>> for NewEither<L, R>
where
    L: NewService<T>,
    R: NewService<U>,
{
    type Service = Either<L::Service, R::Service>;

    fn new_service(&mut self, target: Either<T, U>) -> Self::Service {
        match target {
            Either::A(t) => Either::A(self.left.new_service(t)),
            Either::B(t) => Either::B(self.right.new_service(t)),
        }
    }
}

// === impl Either ===

impl<T, L, R> NewService<T> for Either<L, R>
where
    L: NewService<T>,
    R: NewService<T>,
{
    type Service = Either<L::Service, R::Service>;

    fn new_service(&mut self, target: T) -> Self::Service {
        match self {
            Either::A(n) => Either::A(n.new_service(target)),
            Either::B(n) => Either::B(n.new_service(target)),
        }
    }
}
