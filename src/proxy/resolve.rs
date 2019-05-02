extern crate linkerd2_router as rt;
extern crate tower_discover;

use futures::{Async, Poll};
use std::{
    error, fmt,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

pub use self::tower_discover::Change;
use proxy::Error;
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

pub trait HasEndpointStatus {
    fn endpoint_status(&self) -> EndpointStatus;
}

#[derive(Clone, Debug)]
pub struct EndpointStatus(Arc<AtomicBool>);

#[derive(Clone, Debug)]
pub enum Update<T> {
    Add(SocketAddr, T),
    Remove(SocketAddr),
    NoEndpoints,
}

#[derive(Clone, Debug)]
pub struct Layer<R> {
    resolve: R,
}

#[derive(Clone, Debug)]
pub struct MakeSvc<R, M> {
    resolve: R,
    inner: M,
}

/// Observes an `R`-typed resolution stream, using an `M`-typed endpoint stack to
/// build a service for each endpoint.
#[derive(Clone, Debug)]
pub struct Discover<R, M> {
    resolution: R,
    make: M,
    is_empty: Arc<AtomicBool>,
}

// === impl Layer ===

pub fn layer<T, R>(resolve: R) -> Layer<R>
where
    R: Resolve<T> + Clone,
    R::Endpoint: fmt::Debug,
{
    Layer { resolve }
}

impl<R, M> svc::Layer<M> for Layer<R>
where
    R: Clone,
{
    type Service = MakeSvc<R, M>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeSvc {
            resolve: self.resolve.clone(),
            inner,
        }
    }
}

// === impl MakeSvc ===

impl<T, R, M> svc::Service<T> for MakeSvc<R, M>
where
    R: Resolve<T>,
    R::Endpoint: fmt::Debug,
    M: rt::Make<R::Endpoint> + Clone,
{
    type Response = Discover<R::Resolution, M>;
    type Error = never::Never;
    type Future = futures::future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into()) // always ready to make a Discover
    }

    fn call(&mut self, target: T) -> Self::Future {
        let resolution = self.resolve.resolve(&target);
        futures::future::ok(Discover {
            resolution,
            make: self.inner.clone(),
            is_empty: Arc::new(AtomicBool::new(true)),
        })
    }
}

impl<R, M> HasEndpointStatus for Discover<R, M>
where
    R: Resolution,
{
    fn endpoint_status(&self) -> EndpointStatus {
        EndpointStatus(self.is_empty.clone())
    }
}

impl<R, M> tower_discover::Discover for Discover<R, M>
where
    R: Resolution,
    R::Endpoint: fmt::Debug,
    R::Error: Into<Error>,
    M: rt::Make<R::Endpoint>,
{
    type Key = SocketAddr;
    type Service = M::Value;
    type Error = Error;

    fn poll(&mut self) -> Poll<Change<Self::Key, Self::Service>, Self::Error> {
        loop {
            let up = try_ready!(self.resolution.poll().map_err(Into::into));
            trace!("watch: {:?}", up);
            match up {
                Update::Add(addr, target) => {
                    // We expect the load balancer to handle duplicate inserts
                    // by replacing the old endpoint with the new one, so
                    // insertions of new endpoints and metadata changes for
                    // existing ones can be handled in the same way.
                    let svc = self.make.make(&target);
                    self.is_empty.store(false, Ordering::Release);
                    return Ok(Async::Ready(Change::Insert(addr, svc)));
                }
                Update::Remove(addr) => return Ok(Async::Ready(Change::Remove(addr))),
                Update::NoEndpoints => {
                    self.is_empty.store(true, Ordering::Release);
                    // Keep polling as we should now start to see removals.
                    continue;
                }
            }
        }
    }
}

impl EndpointStatus {
    pub fn is_empty(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}
