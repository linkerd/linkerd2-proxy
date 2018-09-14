use futures::Poll;
use std::net::SocketAddr;

/// Resolves `T`-typed names/addresses as a `Resolution`.
pub trait Resolve<T> {
    type Endpoint;
    type Resolution: Resolution<Endpoint = Self::Endpoint>;

    fn resolve(&self, target: &T) -> Self::Resolution;
}

/// An infinite stream of endpoint updates.
pub trait Resolution {
    type Endpoint;
    type Error;

    fn poll(&mut self) -> Poll<Update<Self::Endpoint>, Self::Error>;
}


#[derive(Debug, Clone)]
pub enum Update<T> {
    Add(SocketAddr, T),
    Remove(SocketAddr),
}
