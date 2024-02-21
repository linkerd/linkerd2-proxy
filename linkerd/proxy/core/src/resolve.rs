use futures::prelude::*;
use linkerd_error::Error;
use std::{
    fmt::Debug,
    net::SocketAddr,
    task::{Context, Poll},
};
use tower::util::Oneshot;

/// Resolves `T`-typed names/addresses as an infinite stream of `Update<Self::Endpoint>`.
pub trait Resolve<T>: Clone + Send + Sync + Unpin + 'static {
    type Endpoint: Clone + Debug + Eq + Send + 'static;
    type Error: Into<Error>;
    type Resolution: Stream<Item = Result<Update<Self::Endpoint>, Self::Error>> + Send + 'static;
    type Future: Future<Output = Result<Self::Resolution, Self::Error>> + Send + Unpin + 'static;

    fn resolve(&self, target: T) -> Self::Future;

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
    T: Send + 'static,
    S: tower::Service<T, Response = R> + Clone + Send + Sync + Unpin + 'static,
    S::Error: Into<Error>,
    S::Future: Send + Unpin + 'static,
    R: Stream<Item = Result<Update<E>, S::Error>> + Send + 'static,
    E: Clone + Debug + Eq + Send + 'static,
{
    type Endpoint = E;
    type Error = S::Error;
    type Resolution = S::Response;
    type Future = Oneshot<S, T>;

    #[inline]
    fn resolve(&self, target: T) -> Self::Future {
        Oneshot::new(self.clone(), target)
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
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&mut self, target: T) -> Self::Future {
        self.0.resolve(target)
    }
}
