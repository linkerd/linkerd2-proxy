use crate::Error;
use futures::{try_ready, Async, Future, Poll, Stream};
use std::net::SocketAddr;
use tower::Service;

pub mod filter;

/// Resolves `T`-typed names/addresses as a `Resolution`.
pub trait Resolve<T> {
    type Endpoint;
    type Error: Into<Error>;
    type Resolution: Resolution<Endpoint = Self::Endpoint>;
    type Future: Future<Item = Self::Resolution, Error = Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error>;

    fn resolve(&mut self, target: T) -> Self::Future;
}

/// An infinite stream of endpoint updates.
pub trait Resolution {
    type Endpoint;
    type Error: Into<Error>;

    fn poll(&mut self) -> Poll<Update<Self::Endpoint>, Self::Error>;
}

#[derive(Clone, Debug)]
pub enum Update<T> {
    Add(SocketAddr, T),
    Remove(SocketAddr),
}

/// Indicates that a stream peer of a resolution was lost.
#[derive(Debug)]
pub struct ResolutionLost(());

// === impl Resolve ===

impl<S, T, R> Resolve<T> for S
where
    S: Service<T, Response = R>,
    S::Error: Into<Error>,
    R: Resolution,
{
    type Endpoint = <R as Resolution>::Endpoint;
    type Error = S::Error;
    type Resolution = S::Response;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Service::poll_ready(self)
    }

    fn resolve(&mut self, target: T) -> Self::Future {
        Service::call(self, target)
    }
}

// === impl Resolution ===

impl<S, N> Resolution for S
where
    S: Stream<Item = Update<N>>,
    S::Error: Into<Error>,
{
    type Endpoint = N;
    type Error = Error;

    fn poll(&mut self) -> Poll<Update<Self::Endpoint>, Self::Error> {
        try_ready!(Stream::poll(self).map_err(Into::into))
            .map(Async::Ready)
            .ok_or_else(|| ResolutionLost(()).into())
    }
}

// === impl ResolutionLost ===

impl std::fmt::Display for ResolutionLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "resolution lost")
    }
}

impl std::error::Error for ResolutionLost {}
