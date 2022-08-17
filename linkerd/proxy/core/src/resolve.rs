use futures::prelude::*;
use linkerd_error::Error;
use std::future::Future;
use std::net::SocketAddr;
use std::task::{Context, Poll};

/// Resolves `T`-typed names/addresses as an infinite stream of `Update<Self::Endpoint>`.
pub trait Resolve<T> {
    type Endpoint;
    type Error: Into<Error>;
    type Resolution: Stream<Item = Result<Update<Self::Endpoint>, Self::Error>>;
    type Future: Future<Output = Result<Self::Resolution, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>>;

    fn resolve(&mut self, target: T) -> Self::Future;

    fn into_service(self) -> ResolveService<Self>
    where
        Self: Sized,
    {
        ResolveService(self)
    }
}

#[derive(Clone, Debug)]
pub struct ResolveService<S>(S);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Update<T> {
    Reset(Vec<(SocketAddr, T)>),
    Add(Vec<(SocketAddr, T)>),
    Remove(Vec<SocketAddr>),
    DoesNotExist,
}

// === impl Resolve ===

impl<S, T, R, E> Resolve<T> for S
where
    S: tower::Service<T, Response = R>,
    S::Error: Into<Error>,
    R: Stream<Item = Result<Update<E>, S::Error>>,
{
    type Endpoint = E;
    type Error = S::Error;
    type Resolution = S::Response;
    type Future = S::Future;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        tower::Service::poll_ready(self, cx)
    }

    #[inline]
    fn resolve(&mut self, target: T) -> Self::Future {
        tower::Service::call(self, target)
    }
}

// === impl Service ===

impl<R, T> tower::Service<T> for ResolveService<R>
where
    R: Resolve<T>,
    R::Error: Into<Error>,
{
    type Error = R::Error;
    type Response = R::Resolution;
    type Future = R::Future;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, target: T) -> Self::Future {
        self.0.resolve(target)
    }
}
