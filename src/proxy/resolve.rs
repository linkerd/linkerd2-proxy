extern crate tower_discover;

use futures::{Async, Poll};
use std::net::SocketAddr;
use std::{error, fmt};

pub use self::tower_discover::Change;
use svc;

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

#[derive(Clone, Debug)]
pub enum Update<T> {
    Add(SocketAddr, T),
    Remove(SocketAddr),
}

#[derive(Clone, Debug)]
pub struct Layer<R> {
    resolve: R,
}

#[derive(Clone, Debug)]
pub struct Stack<R, M> {
    resolve: R,
    inner: M,
}

/// Observes an `R`-typed resolution stream, using an `M`-typed endpoint stack to
/// build a service for each endpoint.
#[derive(Clone, Debug)]
pub struct Discover<R: Resolution, M: svc::Stack<R::Endpoint>> {
    resolution: R,
    make: M,
}

// === impl Layer ===

pub fn layer<T, R>(resolve: R) -> Layer<R>
where
    R: Resolve<T> + Clone,
    R::Endpoint: fmt::Debug,
{
    Layer {
        resolve,
    }
}

impl<T, R, M> svc::Layer<T, R::Endpoint, M> for Layer<R>
where
    R: Resolve<T> + Clone,
    R::Endpoint: fmt::Debug,
    M: svc::Stack<R::Endpoint> + Clone,
{
    type Value = <Stack<R, M> as svc::Stack<T>>::Value;
    type Error = <Stack<R, M> as svc::Stack<T>>::Error;
    type Stack = Stack<R, M>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack {
            resolve: self.resolve.clone(),
            inner,
        }
    }
}

// === impl Stack ===

impl<T, R, M> svc::Stack<T> for Stack<R, M>
where
    R: Resolve<T>,
    R::Endpoint: fmt::Debug,
    M: svc::Stack<R::Endpoint> + Clone,
{
    type Value = Discover<R::Resolution, M>;
    type Error = M::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        let resolution = self.resolve.resolve(target);
        Ok(Discover {
            resolution,
            make: self.inner.clone(),
        })
    }
}

// === impl Discover ===

impl<R, M>  tower_discover::Discover for Discover<R, M>
where
    R: Resolution,
    R::Endpoint: fmt::Debug,
    M: svc::Stack<R::Endpoint>,
{
    type Key = SocketAddr;
    type Service = M::Value;
    type Error = Error<R::Error, M::Error>;

    fn poll(&mut self) -> Poll<Change<Self::Key, Self::Service>, Self::Error> {
        loop {
            let up = try_ready!(self.resolution.poll().map_err(Error::Resolve));
            trace!("watch: {:?}", up);
            match up {
                Update::Add(addr, target) => {
                    // We expect the load balancer to handle duplicate inserts
                    // by replacing the old endpoint with the new one, so
                    // insertions of new endpoints and metadata changes for
                    // existing ones can be handled in the same way.
                    let svc = self.make.make(&target).map_err(Error::Stack)?;
                    return Ok(Async::Ready(Change::Insert(addr, svc)));
                }
                Update::Remove(addr) => {
                    return Ok(Async::Ready(Change::Remove(addr)));
                }
            }
        }
    }
}

// === impl Error ===

#[derive(Debug)]
pub enum Error<R, M> {
    Resolve(R),
    Stack(M),
}

impl<M> fmt::Display for Error<(), M>
where
    M: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Resolve(()) => unreachable!("resolution must succeed"),
            Error::Stack(e) => e.fmt(f),
        }
    }
}

impl<M> error::Error for Error<(), M> where M: error::Error {}
