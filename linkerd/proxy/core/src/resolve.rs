use futures::stream::TryStream;
use linkerd2_error::Error;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Resolves `T`-typed names/addresses as a `Resolution`.
pub trait Resolve<T> {
    type Endpoint;
    type Error: Into<Error>;
    type Resolution: TryStream<Ok = Update<Self::Endpoint>, Error = Self::Error>;
    type Future: Future<Output = Result<Self::Resolution, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>>;

    fn resolve(&mut self, target: T) -> Self::Future;

    fn into_service(self) -> Service<Self>
    where
        Self: Sized,
    {
        Service(self)
    }
}

#[derive(Clone, Debug)]
pub struct Service<S>(S);

#[derive(Clone, Debug, PartialEq)]
pub enum Update<T> {
    Add(Vec<(SocketAddr, T)>),
    Remove(Vec<SocketAddr>),
    Empty,
    DoesNotExist,
}

// === impl Resolve ===

impl<S, T, R, E> Resolve<T> for S
where
    S: tower::Service<T, Response = R>,
    S::Error: Into<Error>,
    R: TryStream<Ok = Update<E>, Error = S::Error>,
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

impl<R, T> tower::Service<T> for Service<R>
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

pub trait ResolutionStreamExt<E, T>: TryStream<Ok = Update<E>, Error = T> {
    fn next_update(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<Update<E>, T>> {
        self.try_poll_next(cx)
            .map(|result| result.expect("resolution stream never ends"))
    }

    fn next_update_pin(&mut self, cx: &mut Context<'_>) -> Poll<Result<Update<E>, T>>
    where
        Self: Unpin,
    {
        Pin::new(self).next_update(cx)
    }
}

impl<E, T, S: TryStream<Ok = Update<E>, Error = T>> ResolutionStreamExt<E, T> for S {}
