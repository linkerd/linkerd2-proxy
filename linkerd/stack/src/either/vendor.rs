//! Contains [`Either`] and related types and functions.
//!
//! See [`Either`] documentation for more details.
//!
//! TODO(kate): this is a lightly modified variant of `tower`'s `Either` service.
//!
//! This is pulled in-tree to punt on addressing breaking changes to the trait bounds of
//! `Either<A, B>`'s `Service` implementation, related to how it no longer boxes the errors
//! returned by its inner services. see #3744.
//!
//! This is vendored from <https://github.com/tower-rs/tower/commit/8b84b98d93a2493422a0ecddb6251f292a904cff>.
//!
//! The variants `A` and `B` have been renamed to `Left` and `Right` to match the names of the v0.5
//! interface.

use futures::ready;
use linkerd_error::Error;
use pin_project::pin_project;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tower::Layer;
use tower::Service;

/// Combine two different service types into a single type.
///
/// Both services must be of the same request, response, and error types.
/// [`Either`] is useful for handling conditional branching in service middleware
/// to different inner service types.
#[pin_project(project = EitherProj)]
#[derive(Clone, Debug)]
pub enum Either<A, B> {
    /// One type of backing [`Service`].
    Left(#[pin] A),
    /// The other type of backing [`Service`].
    Right(#[pin] B),
}

impl<A, B, Request> Service<Request> for Either<A, B>
where
    A: Service<Request>,
    A::Error: Into<Error>,
    B: Service<Request, Response = A::Response>,
    B::Error: Into<Error>,
{
    type Response = A::Response;
    type Error = Error;
    type Future = Either<A::Future, B::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        use self::Either::*;

        match self {
            Left(service) => Poll::Ready(Ok(ready!(service.poll_ready(cx)).map_err(Into::into)?)),
            Right(service) => Poll::Ready(Ok(ready!(service.poll_ready(cx)).map_err(Into::into)?)),
        }
    }

    fn call(&mut self, request: Request) -> Self::Future {
        use self::Either::*;

        match self {
            Left(service) => Left(service.call(request)),
            Right(service) => Right(service.call(request)),
        }
    }
}

impl<A, B, T, AE, BE> Future for Either<A, B>
where
    A: Future<Output = Result<T, AE>>,
    AE: Into<Error>,
    B: Future<Output = Result<T, BE>>,
    BE: Into<Error>,
{
    type Output = Result<T, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            EitherProj::Left(fut) => Poll::Ready(Ok(ready!(fut.poll(cx)).map_err(Into::into)?)),
            EitherProj::Right(fut) => Poll::Ready(Ok(ready!(fut.poll(cx)).map_err(Into::into)?)),
        }
    }
}

impl<S, A, B> Layer<S> for Either<A, B>
where
    A: Layer<S>,
    B: Layer<S>,
{
    type Service = Either<A::Service, B::Service>;

    fn layer(&self, inner: S) -> Self::Service {
        match self {
            Either::Left(layer) => Either::Left(layer.layer(inner)),
            Either::Right(layer) => Either::Right(layer.layer(inner)),
        }
    }
}
