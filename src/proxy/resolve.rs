extern crate linkerd2_router as rt;
extern crate tower_discover;

use futures::{stream::FuturesUnordered, Async, Future, Poll, Stream, sync::oneshot};
use std::{
    fmt,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use indexmap::IndexMap;

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
pub struct Discover<R: Resolution, M: svc::Service<R::Endpoint>> {
    resolution: R,
    make: M,
    pending: Making<M::Future>,
    is_empty: Arc<AtomicBool>,
}

struct Making<F> {
    pending: FuturesUnordered<MakeFuture<F>>,
    cancellations: IndexMap<SocketAddr, oneshot::Sender<()>>,
}

struct MakeFuture<F> {
    inner: F,
    canceled: oneshot::Receiver<()>,
    addr: SocketAddr,
}

enum MakeError<E> {
    Inner(E),
    Canceled,
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
    M: svc::Service<R::Endpoint> + Clone,
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
            pending: Making {
                pending: FuturesUnordered::new(),
                cancellations: IndexMap::new(),
            },
            is_empty: Arc::new(AtomicBool::new(false)),
        })
    }
}

impl<R, M> HasEndpointStatus for Discover<R, M>
where
    R: Resolution,
    M: svc::Service<R::Endpoint>,
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
    M: svc::Service<R::Endpoint>,
    M::Error: Into<Error>,
{
    type Key = SocketAddr;
    type Service = M::Response;
    type Error = Error;

    fn poll(&mut self) -> Poll<Change<Self::Key, Self::Service>, Self::Error> {
        loop {
            if let Async::Ready(Some((addr, svc))) = self.pending.poll().map_err(Into::into)? {
                self.is_empty.store(false, Ordering::Release);
                return Ok(Async::Ready(Change::Insert(addr, svc)));
            }

            try_ready!(self.make.poll_ready().map_err(Into::into));

            let up = try_ready!(self.resolution.poll().map_err(Into::into));
            trace!("watch: {:?}", up);
            match up {
                Update::Add(addr, target) => {
                    let inner = self.make.call(target);
                    self.pending.push(addr, inner);
                }
                Update::Remove(addr) => {
                    self.pending.remove(&addr);
                    return Ok(Async::Ready(Change::Remove(addr)));
                },
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

// ===== impl Making =====

impl<F> Making<F> {
    fn push(&mut self, addr: SocketAddr, inner: F) {
        let (cancel, canceled) = oneshot::channel();
        self.cancellations.insert(addr, cancel);
        self.pending.push(MakeFuture { addr, inner, canceled });
    }

    fn remove(&mut self, addr: &SocketAddr) {
        if let Some(cancel) = self.cancellations.remove(addr) {
            let _ = cancel.send(());
        }
    }
}

impl<F: Future> Stream for Making<F> {
    type Item = (SocketAddr, F::Item);
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            return match self.pending.poll() {
                Err(MakeError::Canceled) => continue,
                Err(MakeError::Inner(err)) => Err(err),
                Ok(Async::Ready(Some((addr, svc)))) => {
                    self.cancellations.remove(&addr);
                    Ok(Async::Ready(Some((addr, svc))))
                },
                Ok(r) => Ok(r),
            }
        }
    }
}

impl<F: Future> Future for MakeFuture<F> {
    type Item = (SocketAddr, F::Item);
    type Error = MakeError<F::Error>;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if let Ok(Async::Ready(())) = self.canceled.poll() {
            trace!("canceled making service for {:?}", self.addr);
            return Err(MakeError::Canceled);
        }
        let svc = try_ready!(self.inner.poll());
        Ok((self.addr, svc).into())
    }
}

impl<E> From<E> for MakeError<E> {
    fn from(inner: E) -> Self {
        MakeError::Inner(inner)
    }
}
