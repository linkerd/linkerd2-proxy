use futures::{Future, Poll};
use std::net::SocketAddr;

pub mod filter;

/// Resolves `T`-typed names/addresses as a `Resolution`.
pub trait Resolve<T> {
    type Endpoint;
    type Future: Future<Item = Self::Resolution>;
    type Resolution: Resolution<Endpoint = Self::Endpoint>;

    /// Asynchronously returns a `Resolution` for the given `target`.
    ///
    /// The returned future will complete with a `Resolution` if this resolver
    /// was able to successfully resolve `target`. Otherwise, if it completes
    /// with an error, that name or address should not be resolved by this
    /// resolver.
    fn resolve(&self, target: &T) -> Self::Future;
}

/// An infinite stream of endpoint updates.
pub trait Resolution {
    type Endpoint;
    type Error;

    fn poll(&mut self) -> Poll<Update<Self::Endpoint>, Self::Error>;
}

#[derive(Clone, Debug)]
pub enum Update<T> {
    Add(SocketAddr, T),
    Remove(SocketAddr),
}
