use futures::{Future, Poll};
use linkerd2_error::Error;
use std::net::SocketAddr;

/// Resolves `T`-typed names/addresses as a `Resolution`.
pub trait Resolve<T> {
    type Endpoint;
    type Error: Into<Error>;
    type Resolution: Resolution<Endpoint = Self::Endpoint>;
    type Future: Future<Item = Self::Resolution, Error = Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error>;

    fn resolve(&mut self, target: T) -> Self::Future;

    fn into_service(self) -> Service<Self>
    where
        Self: Sized,
    {
        Service(self)
    }
}

/// An infinite stream of endpoint updates.
pub trait Resolution {
    type Endpoint;
    type Error: Into<Error>;

    fn poll(&mut self) -> Poll<Update<Self::Endpoint>, Self::Error>;
}

#[derive(Clone, Debug)]
pub struct Service<S>(S);

#[derive(Clone, Debug)]
pub enum Update<T> {
    Add(Vec<(SocketAddr, T)>),
    Remove(Vec<SocketAddr>),
    Empty,
    DoesNotExist,
}

// === impl Resolve ===

impl<S, T, R> Resolve<T> for S
where
    S: tower::Service<T, Response = R>,
    S::Error: Into<Error>,
    R: Resolution,
{
    type Endpoint = <R as Resolution>::Endpoint;
    type Error = S::Error;
    type Resolution = S::Response;
    type Future = S::Future;

    #[inline]
    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        tower::Service::poll_ready(self)
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
    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.0.poll_ready()
    }

    #[inline]
    fn call(&mut self, target: T) -> Self::Future {
        self.0.resolve(target)
    }
}
